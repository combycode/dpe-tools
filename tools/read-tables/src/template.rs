//! Template parser — loads .tpl format files into structured templates.
//!
//! Template syntax:
//!   row_name modifier : cell_pattern ...
//!
//! Modifiers: (none)=once, ?=optional, *=repeat, ~=skip(match but don't emit)
//! Cell patterns: text, number, date, formula, merge, any, empty
//! Specials: ...=rest of row, +=one or more, |=alternatives
//! Total signals: period, total, итого, sparse, sum

use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum RowModifier {
    Once,     // required, exactly once
    Optional, // ? — 0 or 1
    Repeat,   // * — 1 or more
    Skip,     // ~ — match and skip (don't emit)
}

#[derive(Debug, Clone, PartialEq)]
pub enum CellPattern {
    Text,
    Number,
    Date,
    Formula,
    Merge,
    Any,
    Empty,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TotalSignal {
    Period,    // \d{1,2}\.\d{4}:? pattern
    Literal(String), // "total", "итого", etc.
    Sparse,    // significantly fewer filled cells than data rows
    Sum,       // contains SUM/SUBTOTAL formula
}

#[derive(Debug, Clone)]
pub struct RowPattern {
    pub name: String,
    pub modifier: RowModifier,
    pub cells: Vec<CellPattern>,
    pub rest: bool,           // ... at end — accept any remaining cells
    pub one_or_more: bool,    // + on cell type — one or more of that type
    pub total_signals: Vec<TotalSignal>,
}

#[derive(Debug, Clone)]
pub struct Template {
    pub name: String,
    pub rows: Vec<RowPattern>,
}

impl Template {
    /// Parse a template from text content.
    pub fn parse(name: &str, content: &str) -> Result<Self, String> {
        let mut rows = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let (name_part, pattern_part) = line.split_once(':')
                .ok_or_else(|| format!("Invalid template line (no ':'): {}", line))?;

            let name_part = name_part.trim();
            let pattern_part = pattern_part.trim();

            // Parse name and modifier
            let (row_name, modifier) = if let Some(stem) = name_part.strip_suffix('?') {
                (stem.trim().to_string(), RowModifier::Optional)
            } else if let Some(stem) = name_part.strip_suffix('*') {
                (stem.trim().to_string(), RowModifier::Repeat)
            } else if let Some(stem) = name_part.strip_suffix('~') {
                (stem.trim().to_string(), RowModifier::Skip)
            } else {
                (name_part.to_string(), RowModifier::Once)
            };

            // Parse cell patterns
            let mut cells = Vec::new();
            let mut rest = false;
            let mut one_or_more = false;
            let mut total_signals = Vec::new();

            for token in pattern_part.split_whitespace() {
                if token == "..." {
                    rest = true;
                    continue;
                }

                // Handle alternatives with |
                let alternatives: Vec<&str> = token.split('|').collect();

                for alt in &alternatives {
                    let alt = alt.trim();
                    let lower = alt.to_lowercase();
                    // `<type>+` suffix → "one or more of this type", same as
                    // legacy `text+` but generalised to every type so
                    // `data*: date any+` etc. parse properly.
                    let (stripped, plus) = match lower.strip_suffix('+') {
                        Some(s) => (s, true),
                        None    => (lower.as_str(), false),
                    };
                    match stripped {
                        "text"    => cells.push(CellPattern::Text),
                        "number"  => cells.push(CellPattern::Number),
                        "date"    => cells.push(CellPattern::Date),
                        "formula" => cells.push(CellPattern::Formula),
                        "merge"   => cells.push(CellPattern::Merge),
                        "any"     => cells.push(CellPattern::Any),
                        "empty"   => cells.push(CellPattern::Empty),
                        // Total signals — kept as-is (no `+` support).
                        "period" => { total_signals.push(TotalSignal::Period); continue; }
                        "total"  => { total_signals.push(TotalSignal::Literal("total".to_string())); continue; }
                        "итого"  => { total_signals.push(TotalSignal::Literal("итого".to_string())); continue; }
                        "sparse" => { total_signals.push(TotalSignal::Sparse); continue; }
                        "sum"    => { total_signals.push(TotalSignal::Sum); continue; }
                        _ => {
                            // Quoted literal or unknown — treat as literal
                            let cleaned = alt.trim_matches('"');
                            total_signals.push(TotalSignal::Literal(cleaned.to_lowercase()));
                            continue;
                        }
                    }
                    if plus { one_or_more = true; }
                }
            }

            rows.push(RowPattern {
                name: row_name,
                modifier,
                cells,
                rest,
                one_or_more,
                total_signals,
            });
        }

        if rows.is_empty() {
            return Err(format!("Template '{}' has no rows", name));
        }

        Ok(Template { name: name.to_string(), rows })
    }

    /// Load a template from a .tpl file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let name = path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;
        Self::parse(&name, &content)
    }

    /// Load all .tpl files from a directory.
    pub fn load_all(dir: &Path) -> Result<Vec<Self>, String> {
        let mut templates = Vec::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("Cannot read dir {}: {}", dir.display(), e))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("Dir entry error: {}", e))?;
            let path = entry.path();
            if path.extension().map(|e| e == "tpl").unwrap_or(false) {
                templates.push(Self::load(&path)?);
            }
        }
        Ok(templates)
    }

    /// Get the data row pattern (the one with Repeat modifier).
    pub fn data_pattern(&self) -> Option<&RowPattern> {
        self.rows.iter().find(|r| r.modifier == RowModifier::Repeat)
    }

    /// Get the skip/total pattern.
    pub fn total_pattern(&self) -> Option<&RowPattern> {
        self.rows.iter().find(|r| r.modifier == RowModifier::Skip)
    }

    /// Check if template has a title row.
    pub fn has_title(&self) -> bool {
        self.rows.iter().any(|r| r.name == "title")
    }

    /// Check if template has a subheader row.
    pub fn has_subheader(&self) -> bool {
        self.rows.iter().any(|r| r.name == "subheader")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_titled_template() {
        let tpl = Template::parse("titled", "
            title     : merge
            header    : text+
            subheader?: text+
            data*     : any ...
            total~    : period|total|итого|sparse|sum
        ").unwrap();

        assert_eq!(tpl.name, "titled");
        assert_eq!(tpl.rows.len(), 5);

        assert_eq!(tpl.rows[0].name, "title");
        assert_eq!(tpl.rows[0].modifier, RowModifier::Once);
        assert_eq!(tpl.rows[0].cells, vec![CellPattern::Merge]);

        assert_eq!(tpl.rows[1].name, "header");
        assert!(tpl.rows[1].one_or_more);

        assert_eq!(tpl.rows[2].name, "subheader");
        assert_eq!(tpl.rows[2].modifier, RowModifier::Optional);

        assert_eq!(tpl.rows[3].name, "data");
        assert_eq!(tpl.rows[3].modifier, RowModifier::Repeat);
        assert!(tpl.rows[3].rest);

        assert_eq!(tpl.rows[4].name, "total");
        assert_eq!(tpl.rows[4].modifier, RowModifier::Skip);
        assert_eq!(tpl.rows[4].total_signals.len(), 5);
    }

    #[test]
    fn parse_flat_template() {
        let tpl = Template::parse("flat", "
            header : text+
            data*  : any ...
            total~ : period|total|итого|sparse|sum
        ").unwrap();

        assert_eq!(tpl.rows.len(), 3);
        assert!(!tpl.has_title());
        assert!(!tpl.has_subheader());
    }

    #[test]
    fn has_title_check() {
        let titled = Template::parse("t", "title: merge\nheader: text+\ndata*: any ...").unwrap();
        let flat = Template::parse("f", "header: text+\ndata*: any ...").unwrap();
        assert!(titled.has_title());
        assert!(!flat.has_title());
    }

    #[test]
    fn skip_comments() {
        let tpl = Template::parse("t", "# comment\nheader: text+\n# another\ndata*: any ...").unwrap();
        assert_eq!(tpl.rows.len(), 2);
    }

    #[test]
    fn empty_template_errors() {
        assert!(Template::parse("empty", "").is_err());
        assert!(Template::parse("empty", "# only comments").is_err());
    }

    #[test]
    fn type_plus_suffix_parses_for_all_types() {
        // `<type>+` was originally only recognised for `text+`. After the
        // grammar generalisation, every cell type accepts `+`. The `+`
        // sets the row's `one_or_more` flag; the cell type itself is
        // unchanged.
        for (src, expected_kind) in [
            ("date+",    CellPattern::Date),
            ("number+",  CellPattern::Number),
            ("text+",    CellPattern::Text),
            ("formula+", CellPattern::Formula),
            ("merge+",   CellPattern::Merge),
            ("any+",     CellPattern::Any),
            ("empty+",   CellPattern::Empty),
        ] {
            let tpl = Template::parse("t", &format!("data*: {}", src)).unwrap();
            let r = &tpl.rows[0];
            assert_eq!(r.cells, vec![expected_kind.clone()], "src={}", src);
            assert!(r.one_or_more, "src={} should set one_or_more", src);
        }
    }

    #[test]
    fn data_pattern_date_then_any_rest() {
        // The exact shape both shipped templates will adopt.
        let tpl = Template::parse("titled",
            "title: merge\nheader: text+\ndata*: date any ...").unwrap();
        let data = tpl.data_pattern().unwrap();
        assert_eq!(data.cells, vec![CellPattern::Date, CellPattern::Any]);
        assert!(data.rest);
        assert!(!data.one_or_more);
    }

    #[test]
    fn data_pattern_date_then_any_plus() {
        // Alternative shape using `any+` instead of `any ...`.
        let tpl = Template::parse("t", "data*: date any+").unwrap();
        let data = tpl.data_pattern().unwrap();
        assert_eq!(data.cells, vec![CellPattern::Date, CellPattern::Any]);
        assert!(data.one_or_more);
    }

    #[test]
    fn data_pattern_strict_prefix_then_any() {
        // Future enrichment: explicit positional types before the open
        // tail. The matcher uses every non-Any leading cell as a strict
        // positional check; once `Any` (or `...`) appears, the rest of
        // the row is unconstrained.
        let tpl = Template::parse("t", "data*: date number number any+").unwrap();
        let data = tpl.data_pattern().unwrap();
        assert_eq!(
            data.cells,
            vec![CellPattern::Date, CellPattern::Number,
                 CellPattern::Number, CellPattern::Any],
        );
    }
}
