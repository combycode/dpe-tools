//! Filename template parser and resolver for `output.file.name`.
//!
//! Two syntaxes, picked at parse time:
//!
//!   - **Shorthand** — `v.<field>`, matching `^v\.[A-Za-z_][A-Za-z0-9_]*$`.
//!     Resolves to the input envelope's `v.<field>` value, stringified.
//!     The simplest case: `"v.hash"` → `<hash>`.
//!
//!   - **Template** — any string containing `{...}` placeholders. Literal
//!     text outside placeholders is taken as-is. Each placeholder has the
//!     shape `{field}` or `{field:filter1:filter2:...:filterN}` where:
//!       - `field` is a string-or-numeric `v.<field>` key (no `v.` prefix
//!         needed inside `{...}` — `{hash}` means `v.hash`).
//!       - filters are applied left-to-right.
//!       - filters: `safe`, `short`, `nodot`, `lower`, `upper`.
//!       - `short` accepts an optional numeric arg: `short:8` = first 8
//!         chars (default 8). The integer must IMMEDIATELY follow `short`.
//!
//! Filter chain examples:
//!   `{hash}.md`                               → "<hash>.md"
//!   `{hash:short:8}-{filename:safe}.md`       → "<8hex>-<safe_name>.md"
//!   `{directory:safe}/{filename:safe}.md`     → one subdir level; `safe` turns `/` into `_`
//!   `{path:safe}.md`                          → collapsed full path
//!
//! `target_ext` (from settings) is appended only when the resolved name's
//! last path segment has no dot — handled by the caller, not this module.
//! The caller then runs [`check_relative`] so the name cannot leave `dir`.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameTemplate {
    /// `"v.<field>"` shorthand: just look up the named field on the input v.
    Shorthand { field: String },
    /// Mix of literal segments and placeholders with filter chains.
    Template(Vec<Segment>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(String),
    Placeholder { field: String, filters: Vec<Filter> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    Safe,
    Short(usize),
    Nodot,
    Lower,
    Upper,
}

#[derive(Debug)]
pub enum ParseError {
    Empty,
    UnbalancedBrace { at: usize },
    EmptyPlaceholder,
    UnknownFilter { name: String },
    NumericNotAfterShort { value: String },
    InvalidShortArg { value: String },
    InvalidFieldName { name: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "filename template is empty"),
            Self::UnbalancedBrace { at } =>
                write!(f, "filename template: unbalanced '{{' at byte {}", at),
            Self::EmptyPlaceholder => write!(f, "filename template: empty `{{}}` placeholder"),
            Self::UnknownFilter { name } =>
                write!(f, "filename template: unknown filter '{}' (valid: safe, short, nodot, lower, upper)", name),
            Self::NumericNotAfterShort { value } =>
                write!(f, "filename template: numeric token '{}' must follow `short` filter", value),
            Self::InvalidShortArg { value } =>
                write!(f, "filename template: `short:{}` arg must be a positive integer", value),
            Self::InvalidFieldName { name } =>
                write!(f, "filename template: invalid field name '{}' inside placeholder", name),
        }
    }
}
impl std::error::Error for ParseError {}

#[derive(Debug)]
pub enum ResolveError {
    MissingField { name: String },
    NotStringifiable { name: String },
    EmptyValue { name: String },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField { name } =>
                write!(f, "filename template: envelope has no field '{}'", name),
            Self::NotStringifiable { name } =>
                write!(f, "filename template: envelope field '{}' is not string-or-numeric", name),
            Self::EmptyValue { name } =>
                write!(f, "filename template: envelope field '{}' is empty", name),
        }
    }
}
impl std::error::Error for ResolveError {}

impl NameTemplate {
    /// Parse `name` settings into the structured form. Picks shorthand
    /// when the string matches `v.<ident>` exactly; otherwise treats as
    /// a template (which may or may not contain `{...}` — a literal
    /// template like `"out.md"` is valid and resolves to itself).
    pub fn parse(name: &str) -> Result<Self, ParseError> {
        if name.is_empty() {
            return Err(ParseError::Empty);
        }
        if let Some(field) = shorthand_field(name) {
            return Ok(Self::Shorthand { field });
        }
        Ok(Self::Template(parse_template(name)?))
    }

    /// Resolve against an input envelope's `v` object. Returns the
    /// computed filename string. Errors surface field-resolution
    /// failures (missing / wrong-typed / empty).
    pub fn resolve(&self, v: &Value) -> Result<String, ResolveError> {
        match self {
            Self::Shorthand { field } => resolve_field(v, field),
            Self::Template(segments) => {
                let mut out = String::new();
                for seg in segments {
                    match seg {
                        Segment::Literal(s) => out.push_str(s),
                        Segment::Placeholder { field, filters } => {
                            let raw = resolve_field(v, field)?;
                            let value = apply_filters(&raw, filters);
                            if value.is_empty() {
                                return Err(ResolveError::EmptyValue { name: field.clone() });
                            }
                            out.push_str(&value);
                        }
                    }
                }
                Ok(out)
            }
        }
    }
}

// ─── shorthand detection ──────────────────────────────────────────────

/// Match `^v\.[A-Za-z_][A-Za-z0-9_]*$` without pulling in regex.
fn shorthand_field(name: &str) -> Option<String> {
    let rest = name.strip_prefix("v.")?;
    if !is_valid_field_name(rest) { return None; }
    Some(rest.to_string())
}

// ─── template parser ──────────────────────────────────────────────────

fn parse_template(s: &str) -> Result<Vec<Segment>, ParseError> {
    let bytes = s.as_bytes();
    let mut out: Vec<Segment> = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'{' {
            if !lit.is_empty() {
                out.push(Segment::Literal(std::mem::take(&mut lit)));
            }
            let close = find_closing_brace(bytes, i)
                .ok_or(ParseError::UnbalancedBrace { at: i })?;
            let inner = &s[i + 1..close];
            if inner.is_empty() {
                return Err(ParseError::EmptyPlaceholder);
            }
            out.push(parse_placeholder(inner)?);
            i = close + 1;
        } else if c == b'}' {
            // Stray closing brace — treat as literal.
            lit.push('}');
            i += 1;
        } else {
            // Push one UTF-8 code point at a time. Bytes are valid UTF-8
            // because `s` is `&str`.
            let ch_len = utf8_char_len(c);
            lit.push_str(&s[i..i + ch_len]);
            i += ch_len;
        }
    }
    if !lit.is_empty() {
        out.push(Segment::Literal(lit));
    }
    Ok(out)
}

fn utf8_char_len(first_byte: u8) -> usize {
    // < 0xC0 covers ASCII (1 byte) AND stray continuation bytes (also
    // treated as 1 to advance safely — shouldn't happen at a real code-
    // point boundary but be defensive).
    if first_byte < 0xC0 { 1 }
    else if first_byte < 0xE0 { 2 }
    else if first_byte < 0xF0 { 3 }
    else { 4 }
}

fn find_closing_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut j = open + 1;
    while j < bytes.len() {
        if bytes[j] == b'}' { return Some(j); }
        if bytes[j] == b'{' { return None; }  // nested `{` not supported
        j += 1;
    }
    None
}

fn parse_placeholder(inner: &str) -> Result<Segment, ParseError> {
    let mut parts = inner.split(':');
    let field = parts.next().unwrap();          // split always yields at least one
    if field.is_empty() {
        return Err(ParseError::EmptyPlaceholder);
    }
    if !is_valid_field_name(field) {
        return Err(ParseError::InvalidFieldName { name: field.to_string() });
    }

    let mut filters: Vec<Filter> = Vec::new();
    for token in parts {
        // Numeric token? Must follow `short` filter (whose default arg
        // we then override). Otherwise reject.
        if let Ok(n) = token.parse::<usize>() {
            match filters.last_mut() {
                Some(Filter::Short(slot)) => { *slot = n; }
                _ => {
                    return Err(ParseError::NumericNotAfterShort {
                        value: token.to_string(),
                    });
                }
            }
            continue;
        }
        match token {
            "safe"  => filters.push(Filter::Safe),
            "short" => filters.push(Filter::Short(8)),
            "nodot" => filters.push(Filter::Nodot),
            "lower" => filters.push(Filter::Lower),
            "upper" => filters.push(Filter::Upper),
            other   => return Err(ParseError::UnknownFilter { name: other.to_string() }),
        }
    }

    // Validate short args: 0 means truncate-to-empty — almost certainly
    // a typo. Reject at parse time.
    for f in &filters {
        if let Filter::Short(0) = f {
            return Err(ParseError::InvalidShortArg { value: "0".to_string() });
        }
    }

    Ok(Segment::Placeholder { field: field.to_string(), filters })
}

fn is_valid_field_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ─── field resolution ─────────────────────────────────────────────────

fn resolve_field(v: &Value, field: &str) -> Result<String, ResolveError> {
    let raw = v.get(field).ok_or_else(|| ResolveError::MissingField {
        name: field.to_string(),
    })?;
    let s = match raw {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b)   => b.to_string(),
        _                => return Err(ResolveError::NotStringifiable {
            name: field.to_string(),
        }),
    };
    if s.is_empty() {
        return Err(ResolveError::EmptyValue { name: field.to_string() });
    }
    Ok(s)
}

// ─── filter application ───────────────────────────────────────────────

fn apply_filters(value: &str, filters: &[Filter]) -> String {
    let mut s = value.to_string();
    for f in filters {
        s = apply_one(&s, f);
    }
    s
}

fn apply_one(value: &str, f: &Filter) -> String {
    match f {
        Filter::Safe  => safe(value),
        Filter::Short(n) => short(value, *n),
        Filter::Nodot => nodot(value),
        Filter::Lower => value.to_lowercase(),
        Filter::Upper => value.to_uppercase(),
    }
}

fn safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            // Preserve case for alphanumerics here; lowercase happens below.
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out.to_lowercase()
}

fn short(s: &str, n: usize) -> String {
    // `n` is char-count not byte-count: stable for non-ASCII fields.
    s.chars().take(n).collect()
}

fn nodot(s: &str) -> String {
    match s.rfind('.') {
        Some(i) => s[..i].to_string(),
        None    => s.to_string(),
    }
}

// ─── helpers exposed to the caller (target_ext rule) ──────────────────

/// Whether the resolved name's LAST path segment has a dot. The caller
/// uses this to decide whether to append `target_ext`. "Last segment"
/// is the chunk after the final `/` or `\` (forward and backslash both
/// treated as separators — Windows-friendly).
pub fn last_segment_has_dot(name: &str) -> bool {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    last.contains('.')
}

/// Reject a resolved name that would land outside the output `dir` once
/// joined to it: an absolute path or drive (`/x`, `\x`, `C:/x`, `C:x`),
/// an empty segment, or a `.` / `..` segment. Both `/` and `\` count as
/// separators on every OS, matching [`last_segment_has_dot`].
pub fn check_relative(name: &str) -> Result<(), String> {
    if name.contains(':') {
        return Err(format!("resolved name '{}' contains ':' (drive or stream syntax)", name));
    }
    for seg in name.split(['/', '\\']) {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!(
                "resolved name '{}' must be a relative path inside output.file.dir \
                 (no leading separator, empty, '.' or '..' segments)",
                name
            ));
        }
    }
    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─── parse: shorthand ─────────────────────────────────────────────

    #[test]
    fn parse_shorthand_simple() {
        let t = NameTemplate::parse("v.hash").unwrap();
        assert_eq!(t, NameTemplate::Shorthand { field: "hash".into() });
    }

    #[test]
    fn parse_shorthand_underscore_field() {
        let t = NameTemplate::parse("v.source_hash").unwrap();
        assert_eq!(t, NameTemplate::Shorthand { field: "source_hash".into() });
    }

    #[test]
    fn parse_shorthand_no_template_braces() {
        let t = NameTemplate::parse("v.path").unwrap();
        matches!(t, NameTemplate::Shorthand { .. });
    }

    #[test]
    fn parse_v_dot_field_with_invalid_char_falls_to_template() {
        // `v.hash-x` doesn't match shorthand regex (dash) → template.
        // It parses as a literal "v.hash-x" since no `{` is present.
        let t = NameTemplate::parse("v.hash-x").unwrap();
        if let NameTemplate::Template(segs) = t {
            assert_eq!(segs, vec![Segment::Literal("v.hash-x".into())]);
        } else {
            panic!("expected template");
        }
    }

    // ─── parse: template ──────────────────────────────────────────────

    #[test]
    fn parse_template_pure_literal() {
        let t = NameTemplate::parse("hello.md").unwrap();
        if let NameTemplate::Template(segs) = t {
            assert_eq!(segs, vec![Segment::Literal("hello.md".into())]);
        } else {
            panic!("expected template");
        }
    }

    #[test]
    fn parse_template_single_placeholder() {
        let t = NameTemplate::parse("{hash}.md").unwrap();
        if let NameTemplate::Template(segs) = t {
            assert_eq!(segs.len(), 2);
            assert!(matches!(&segs[0], Segment::Placeholder { field, filters }
                if field == "hash" && filters.is_empty()));
            assert_eq!(segs[1], Segment::Literal(".md".into()));
        } else {
            panic!("expected template");
        }
    }

    #[test]
    fn parse_template_with_filters() {
        let t = NameTemplate::parse("{filename:safe}.md").unwrap();
        if let NameTemplate::Template(segs) = t {
            assert!(matches!(&segs[0], Segment::Placeholder { field, filters }
                if field == "filename" && filters == &vec![Filter::Safe]));
        } else {
            panic!("expected template");
        }
    }

    #[test]
    fn parse_template_chained_filters() {
        let t = NameTemplate::parse("{path:safe:short}.md").unwrap();
        if let NameTemplate::Template(segs) = t {
            assert!(matches!(&segs[0], Segment::Placeholder { field, filters }
                if field == "path" && filters == &vec![Filter::Safe, Filter::Short(8)]));
        } else {
            panic!("expected template");
        }
    }

    #[test]
    fn parse_template_short_with_explicit_n() {
        let t = NameTemplate::parse("{hash:short:6}").unwrap();
        if let NameTemplate::Template(segs) = t {
            assert!(matches!(&segs[0], Segment::Placeholder { field, filters }
                if field == "hash" && filters == &vec![Filter::Short(6)]));
        } else {
            panic!("expected template");
        }
    }

    #[test]
    fn parse_template_short_with_n_then_more_filters() {
        let t = NameTemplate::parse("{hash:short:6:safe}").unwrap();
        if let NameTemplate::Template(segs) = t {
            assert!(matches!(&segs[0], Segment::Placeholder { field, filters }
                if field == "hash" && filters == &vec![Filter::Short(6), Filter::Safe]));
        } else {
            panic!("expected template");
        }
    }

    #[test]
    fn parse_template_multiple_placeholders() {
        let t = NameTemplate::parse("{hash:short:8}-{filename:safe}.md").unwrap();
        if let NameTemplate::Template(segs) = t {
            assert_eq!(segs.len(), 4);
            assert!(matches!(&segs[0], Segment::Placeholder { field, .. } if field == "hash"));
            assert_eq!(segs[1], Segment::Literal("-".into()));
            assert!(matches!(&segs[2], Segment::Placeholder { field, .. } if field == "filename"));
            assert_eq!(segs[3], Segment::Literal(".md".into()));
        } else {
            panic!("expected template");
        }
    }

    #[test]
    fn parse_template_unbalanced_brace() {
        let r = NameTemplate::parse("{hash.md");
        assert!(matches!(r, Err(ParseError::UnbalancedBrace { .. })));
    }

    #[test]
    fn parse_template_empty_placeholder() {
        let r = NameTemplate::parse("{}.md");
        assert!(matches!(r, Err(ParseError::EmptyPlaceholder)));
    }

    #[test]
    fn parse_template_unknown_filter() {
        let r = NameTemplate::parse("{hash:reverse}.md");
        assert!(matches!(r, Err(ParseError::UnknownFilter { name }) if name == "reverse"));
    }

    #[test]
    fn parse_template_numeric_outside_short() {
        let r = NameTemplate::parse("{hash:safe:5}.md");
        assert!(matches!(r, Err(ParseError::NumericNotAfterShort { .. })));
    }

    #[test]
    fn parse_template_short_zero_rejected() {
        let r = NameTemplate::parse("{hash:short:0}");
        assert!(matches!(r, Err(ParseError::InvalidShortArg { .. })));
    }

    #[test]
    fn parse_template_invalid_field_name() {
        let r = NameTemplate::parse("{my-field}.md");
        assert!(matches!(r, Err(ParseError::InvalidFieldName { .. })));
    }

    #[test]
    fn parse_empty_string() {
        let r = NameTemplate::parse("");
        assert!(matches!(r, Err(ParseError::Empty)));
    }

    // ─── resolve ─────────────────────────────────────────────────────

    #[test]
    fn resolve_shorthand_hash() {
        let t = NameTemplate::parse("v.hash").unwrap();
        let v = json!({"hash": "abc123def"});
        assert_eq!(t.resolve(&v).unwrap(), "abc123def");
    }

    #[test]
    fn resolve_shorthand_missing() {
        let t = NameTemplate::parse("v.hash").unwrap();
        let v = json!({"path": "/p"});
        assert!(matches!(t.resolve(&v), Err(ResolveError::MissingField { .. })));
    }

    #[test]
    fn resolve_shorthand_empty() {
        let t = NameTemplate::parse("v.hash").unwrap();
        let v = json!({"hash": ""});
        assert!(matches!(t.resolve(&v), Err(ResolveError::EmptyValue { .. })));
    }

    #[test]
    fn resolve_shorthand_non_string() {
        let t = NameTemplate::parse("v.size").unwrap();
        let v = json!({"size": 12345});
        assert_eq!(t.resolve(&v).unwrap(), "12345");
    }

    #[test]
    fn resolve_template_basic() {
        let t = NameTemplate::parse("{hash}.md").unwrap();
        let v = json!({"hash": "abc"});
        assert_eq!(t.resolve(&v).unwrap(), "abc.md");
    }

    #[test]
    fn resolve_template_safe_filter() {
        let t = NameTemplate::parse("{filename:safe}.md").unwrap();
        let v = json!({"filename": "Q4 2025 / Report (final).pdf"});
        // Spaces, slash, parens → underscores; lowercased.
        let r = t.resolve(&v).unwrap();
        assert_eq!(r, "q4_2025___report__final_.pdf.md");
    }

    #[test]
    fn resolve_template_short_default() {
        let t = NameTemplate::parse("{hash:short}").unwrap();
        let v = json!({"hash": "abcdef1234567890"});
        assert_eq!(t.resolve(&v).unwrap(), "abcdef12");
    }

    #[test]
    fn resolve_template_short_explicit_n() {
        let t = NameTemplate::parse("{hash:short:4}").unwrap();
        let v = json!({"hash": "abcdef1234567890"});
        assert_eq!(t.resolve(&v).unwrap(), "abcd");
    }

    #[test]
    fn resolve_template_chained() {
        let t = NameTemplate::parse("{hash:short:8}-{filename:safe}.md").unwrap();
        let v = json!({
            "hash": "abcdef1234567890",
            "filename": "Q4 Report.pdf",
        });
        assert_eq!(t.resolve(&v).unwrap(), "abcdef12-q4_report.pdf.md");
    }

    #[test]
    fn resolve_template_nodot() {
        let t = NameTemplate::parse("{filename:nodot}.md").unwrap();
        let v = json!({"filename": "report.pdf"});
        assert_eq!(t.resolve(&v).unwrap(), "report.md");
    }

    #[test]
    fn resolve_template_lower_upper() {
        let lower = NameTemplate::parse("{name:lower}").unwrap();
        let upper = NameTemplate::parse("{name:upper}").unwrap();
        let v = json!({"name": "Hello"});
        assert_eq!(lower.resolve(&v).unwrap(), "hello");
        assert_eq!(upper.resolve(&v).unwrap(), "HELLO");
    }

    #[test]
    fn resolve_template_safe_then_short() {
        let t = NameTemplate::parse("{filename:safe:short:5}").unwrap();
        let v = json!({"filename": "MIXED CASE.pdf"});
        // safe → "mixed_case.pdf", then short(5) → "mixed"
        assert_eq!(t.resolve(&v).unwrap(), "mixed");
    }

    #[test]
    fn resolve_template_literal_only() {
        let t = NameTemplate::parse("constant.md").unwrap();
        let v = json!({});
        assert_eq!(t.resolve(&v).unwrap(), "constant.md");
    }

    // ─── last-segment-has-dot helper ──────────────────────────────────

    #[test]
    fn last_segment_has_dot_basic() {
        assert!(last_segment_has_dot("foo.md"));
        assert!(!last_segment_has_dot("foo"));
        assert!(last_segment_has_dot("sub/foo.md"));
        assert!(!last_segment_has_dot("sub/foo"));
        assert!(!last_segment_has_dot("a.b/c"));     // last segment 'c' has no dot
        assert!(last_segment_has_dot("a.b/c.d"));
        assert!(!last_segment_has_dot(""));
    }

    #[test]
    fn check_relative_accepts_names_and_subdirs() {
        for ok in ["abc.md", "sub/abc.md", "sub\\abc.md", "a.b/c", "_in_2024/foo.pdf.md"] {
            assert!(check_relative(ok).is_ok(), "{}", ok);
        }
    }

    #[test]
    fn check_relative_rejects_escapes() {
        for bad in [
            "/data/in/foo.pdf", "\\\\srv\\share\\x.md", "C:/in/x.pdf", "C:x.md",
            "../x.md", "a/../../x.md", "..", ".", "a//b.md", "a/", "",
        ] {
            assert!(check_relative(bad).is_err(), "{}", bad);
        }
    }

    #[test]
    fn resolve_empty_after_filters_errors() {
        let t = NameTemplate::parse("{filename:nodot}").unwrap();
        let err = t.resolve(&json!({"filename": ".gitignore"})).unwrap_err();
        assert!(matches!(err, ResolveError::EmptyValue { .. }), "{:?}", err);
    }

    #[test]
    fn last_segment_has_dot_windows() {
        assert!(last_segment_has_dot("dir\\file.md"));
        assert!(!last_segment_has_dot("dir\\file"));
    }
}
