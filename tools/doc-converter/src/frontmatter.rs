//! YAML frontmatter parse + reverse-priority merge.
//!
//! Shape of a page response (possibly):
//!   ---
//!   doc_id: DOC-001
//!   doc_date: 01.01.2024
//!   ---
//!   <body>
//!
//! For each page we run `extract(body)` → (frontmatter_map, body_without_fm).
//! Then across pages 1..N we `merge_reverse_priority`: earlier pages win;
//! later pages only add keys that weren't set yet.

use std::collections::BTreeMap;

use serde_json::Value;

/// Parse the leading `---\n...\n---` frontmatter if present.
/// Returns (fields_as_string_map, body_without_frontmatter).
/// Missing / malformed frontmatter → (empty, original_text).
pub fn extract(text: &str) -> (BTreeMap<String, String>, String) {
    let trimmed = text.trim_start_matches(['\u{feff}']).trim_start_matches('\n');
    // Must start with "---" on its own line.
    if !(trimmed.starts_with("---\n") || trimmed.starts_with("---\r\n")) {
        return (BTreeMap::new(), text.to_string());
    }
    // Skip the opening "---" line.
    let (_, after_open) = trimmed.split_once('\n').unwrap();
    // Find the closing "---" line (preceded by newline).
    let close_pos = find_closing_fence(after_open);
    let Some((fm_text, rest)) = close_pos.map(|i| split_at_fence(after_open, i)) else {
        return (BTreeMap::new(), text.to_string());
    };

    let map = parse_yaml_flat(fm_text).unwrap_or_default();
    (map, rest.trim_start_matches('\n').to_string())
}

/// Merge many per-page frontmatters with reverse priority: earlier entries
/// dominate; later entries fill in missing keys only (no overwrite).
pub fn merge_reverse_priority(
    maps: &[BTreeMap<String, String>],
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for m in maps {
        for (k, v) in m {
            out.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    out
}

/// Serialize a merged frontmatter back to a block suitable for prepending
/// to combined markdown. Preserves BTreeMap key order.
pub fn render_block(fm: &BTreeMap<String, String>) -> String {
    if fm.is_empty() { return String::new(); }
    let mut s = String::from("---\n");
    for (k, v) in fm {
        s.push_str(k);
        s.push_str(": ");
        s.push_str(v);
        s.push('\n');
    }
    s.push_str("---\n");
    s
}

// ─── internals ───────────────────────────────────────────────────────

fn find_closing_fence(after_open: &str) -> Option<usize> {
    // Look for "\n---\n" or "\n---\r\n" or "\n---" at end.
    let mut start = 0;
    while let Some(idx) = after_open[start..].find("---") {
        let abs = start + idx;
        // Must be at start of line (abs == 0 or preceded by '\n').
        let at_line_start = abs == 0 || after_open.as_bytes().get(abs - 1) == Some(&b'\n');
        // Followed by end-of-line or EOF.
        let after = abs + 3;
        let at_line_end = after >= after_open.len()
            || after_open.as_bytes().get(after) == Some(&b'\n')
            || after_open.as_bytes().get(after) == Some(&b'\r');
        if at_line_start && at_line_end {
            return Some(abs);
        }
        start = abs + 3;
    }
    None
}

/// Split `after_open` around the fence at `close_pos`, returning (fm_text, rest).
fn split_at_fence(after_open: &str, close_pos: usize) -> (&str, &str) {
    let fm_text = &after_open[..close_pos];
    let rest_start = close_pos + 3;
    let mut rest = &after_open[rest_start..];
    if let Some(s) = rest.strip_prefix('\r') { rest = s; }
    if let Some(s) = rest.strip_prefix('\n') { rest = s; }
    (fm_text, rest)
}

/// Parse a flat key:value YAML block (no nesting). Values are read as strings.
/// Returns None if YAML is structurally broken.
///
/// Uses serde-saphyr in strict YAML 1.2 mode (workspace policy). Dates,
/// times, and YAML 1.1 boolean quirks (`yes`/`no`/`on`/`off`) are left as
/// strings, which is the correct behavior for frontmatter where values
/// are downstream-merged as text.
fn parse_yaml_flat(s: &str) -> Option<BTreeMap<String, String>> {
    let opts = serde_saphyr::options!(
        strict_booleans:      true,
        no_schema:            true,
        legacy_octal_numbers: false,
        // keep .inf/.nan/-.inf as strings, as serde-saphyr 0.0.x did
        non_finite_float_policy: serde_saphyr::options::NonFiniteFloatPolicy::AsString,
        // deeper YAML is a clean error; ~250 levels overflow a 1 MB (Windows main-thread) stack
        budget: serde_saphyr::budget!(max_depth: 128),
    );
    let value: Value = serde_saphyr::from_str_with_options::<Value>(s, opts).ok()?;
    let obj = match value {
        Value::Object(m) => m,
        // Empty or non-mapping frontmatter is treated as no fields.
        _ => return Some(BTreeMap::new()),
    };
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        out.insert(k, json_value_to_string(&v));
    }
    Some(out)
}

fn json_value_to_string(v: &Value) -> String {
    match v {
        Value::Null      => String::new(),
        Value::Bool(b)   => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // Frontmatter is documented as flat. Nested values are an edge
        // case; emit as compact JSON (valid YAML flow style) so round-
        // trips through downstream YAML readers still parse.
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(v).unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn extract_no_frontmatter_returns_empty_and_original_body() {
        let (fm, body) = extract("# heading\nlorem");
        assert!(fm.is_empty());
        assert_eq!(body, "# heading\nlorem");
    }

    #[test]
    fn extract_basic_frontmatter() {
        let text = "---\ndoc_id: DOC-001\ndoc_date: 01.01.2024\n---\n# Body\n";
        let (fm, body) = extract(text);
        assert_eq!(fm.get("doc_id").unwrap(), "DOC-001");
        assert_eq!(fm.get("doc_date").unwrap(), "01.01.2024");
        assert_eq!(body, "# Body\n");
    }

    #[test]
    fn extract_leading_bom_and_newlines_tolerated() {
        let text = "\u{feff}\n---\na: 1\n---\nbody";
        let (fm, _) = extract(text);
        assert_eq!(fm.get("a").unwrap(), "1");
    }

    #[test]
    fn extract_crlf_tolerated() {
        let text = "---\r\na: 1\r\n---\r\nbody";
        let (fm, _body) = extract(text);
        assert_eq!(fm.get("a").unwrap(), "1");
    }

    #[test]
    fn extract_missing_close_returns_no_fm() {
        let text = "---\na: 1\nno closing fence";
        let (fm, body) = extract(text);
        assert!(fm.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn extract_not_starting_with_dashes() {
        let text = "body\n---\na: 1\n---";
        let (fm, body) = extract(text);
        assert!(fm.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn merge_first_map_wins() {
        let merged = merge_reverse_priority(&[
            m(&[("a", "1"), ("b", "2")]),
            m(&[("a", "999"), ("c", "3")]),
            m(&[("b", "888"), ("c", "777"), ("d", "4")]),
        ]);
        assert_eq!(merged, m(&[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")]));
    }

    #[test]
    fn merge_empty_inputs_yields_empty() {
        assert!(merge_reverse_priority(&[]).is_empty());
    }

    #[test]
    fn render_block_roundtrip() {
        let fm = m(&[("a", "1"), ("b", "two words")]);
        let block = render_block(&fm);
        assert!(block.starts_with("---\n"));
        assert!(block.ends_with("---\n"));
        let reparsed = extract(&format!("{}body", block));
        assert_eq!(reparsed.0, fm);
        assert_eq!(reparsed.1, "body");
    }

    #[test]
    fn render_empty_is_empty_string() {
        assert_eq!(render_block(&BTreeMap::new()), "");
    }

    #[test]
    fn yaml_values_numeric_and_date_as_strings() {
        // dates and numbers come out as their original string form
        let text = "---\ndoc_date: 2024-07-31\nchapter_id: 1\n---\n";
        let (fm, _) = extract(text);
        assert_eq!(fm.get("doc_date").unwrap(), "2024-07-31");
        assert_eq!(fm.get("chapter_id").unwrap(), "1");
    }
}
