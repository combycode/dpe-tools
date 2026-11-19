//! Low-level XLSX XML parsing: cells, shared strings, styles, merges.

use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::{BytesRef, BytesText, Event};
use quick_xml::Reader;
use std::collections::HashMap;
use std::io::{BufReader, Read};
use zip::ZipArchive;

// ── Data Structures ──

#[derive(Clone, Debug)]
pub struct CellData {
    pub row: usize,
    pub col: usize,
    pub value: Option<String>,
    pub formula: Option<String>,
    pub cell_type: String,
    pub style_idx: usize,
}

pub struct StyleInfo {
    pub is_date: Vec<bool>,
}

impl StyleInfo {
    pub fn is_date_style(&self, idx: usize) -> bool {
        self.is_date.get(idx).copied().unwrap_or(false)
    }
}

#[derive(Clone, Debug)]
pub struct RawMerge {
    pub min_row: usize,
    pub min_col: usize,
    pub max_row: usize,
    pub max_col: usize,
}

pub struct SheetData {
    pub cells: Vec<CellData>,
    pub merges: Vec<RawMerge>,
    pub max_row: usize,
    pub max_col: usize,
    pub rows_with_sum: std::collections::HashSet<usize>,
    cell_map: HashMap<(usize, usize), usize>,
}

impl SheetData {
    /// Build SheetData from a list of cells and merges. Used in tests.
    pub fn from_cells(cells: Vec<CellData>, merges: Vec<RawMerge>) -> Self {
        let mut max_row = 0;
        let mut max_col = 0;
        let mut cell_map = HashMap::with_capacity(cells.len());
        let mut rows_with_sum = std::collections::HashSet::new();

        for (i, cd) in cells.iter().enumerate() {
            max_row = max_row.max(cd.row + 1);
            max_col = max_col.max(cd.col + 1);
            cell_map.insert((cd.row, cd.col), i);
            if let Some(ref f) = cd.formula {
                let upper = f.to_uppercase();
                if upper.contains("SUM(") || upper.contains("SUBTOTAL(") {
                    rows_with_sum.insert(cd.row);
                }
            }
        }

        Self { cells, merges, max_row, max_col, rows_with_sum, cell_map }
    }

    pub fn get_cell(&self, row: usize, col: usize) -> Option<&CellData> {
        self.cell_map.get(&(row, col)).map(|&i| &self.cells[i])
    }

    pub fn get_value(&self, row: usize, col: usize) -> Option<&str> {
        self.get_cell(row, col).and_then(|c| c.value.as_deref())
    }

    pub fn has_data(&self, row: usize, col: usize) -> bool {
        self.cell_map.contains_key(&(row, col))
    }

    pub fn row_fill_count(&self, row: usize, start_col: usize, end_col: usize) -> usize {
        (start_col..=end_col).filter(|&c| self.has_data(row, c)).count()
    }

    /// Check if a cell has a formula (not a value-only cell)
    pub fn has_formula(&self, row: usize, col: usize) -> bool {
        self.get_cell(row, col).map(|c| c.formula.is_some()).unwrap_or(false)
    }

    /// Check if a cell is a text string (shared string type, no formula)
    pub fn is_text_cell(&self, row: usize, col: usize) -> bool {
        match self.get_cell(row, col) {
            Some(c) => c.formula.is_none() && c.cell_type == "s",
            None => false,
        }
    }

    /// Check if a cell has a numeric value (not string, not formula-only)
    pub fn is_numeric_cell(&self, row: usize, col: usize) -> bool {
        match self.get_cell(row, col) {
            Some(c) => {
                if c.formula.is_some() { return true; } // formulas produce numbers
                if c.cell_type == "s" || c.cell_type == "inlineStr" { return false; }
                c.value.as_ref().map(|v| v.parse::<f64>().is_ok()).unwrap_or(false)
            }
            None => false,
        }
    }
}

// ── Cell Reference Parsing ──

pub fn parse_cell_ref(r: &str) -> (usize, usize) {
    let mut col = 0usize;
    let mut row_str = String::new();
    for ch in r.chars() {
        if ch.is_ascii_alphabetic() {
            col = col * 26 + (ch.to_ascii_uppercase() as usize - 'A' as usize + 1);
        } else {
            row_str.push(ch);
        }
    }
    let row = row_str.parse::<usize>().unwrap_or(1);
    (row.saturating_sub(1), col.saturating_sub(1))
}

fn parse_range_str(s: &str) -> Option<((usize, usize), (usize, usize))> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 { return None; }
    Some((parse_cell_ref(parts[0]), parse_cell_ref(parts[1])))
}

// ── Text Content ──

/// Append a text event's content as-is (no EOL normalization, as before quick-xml 0.38).
fn push_text(out: &mut String, e: &BytesText<'_>) {
    out.push_str(e);
}

/// Append an entity reference (`&amp;`, `&#10;`, ...). Since quick-xml 0.38 these arrive as
/// separate `GeneralRef` events instead of inside the text. Unknown entities are dropped.
fn push_ref(out: &mut String, r: &BytesRef<'_>) {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        out.push(c);
    } else if let Some(s) = resolve_xml_entity(r) {
        out.push_str(s);
    }
}

// ── Lenient UTF-8 ──

/// Streams a part's bytes with invalid UTF-8 sequences replaced by U+FFFD.
///
/// quick-xml >= 0.38 validates UTF-8 on every event and cannot resume after a failure, so one
/// bad byte from a third-party writer would cut off the rest of the part. Before, only the
/// affected text run was lost; now it keeps its readable characters.
pub struct LossyUtf8<R> {
    inner: R,
    chunk: usize,
    out: Vec<u8>,
    pos: usize,
    carry: Vec<u8>,
    eof: bool,
}

impl<R: Read> LossyUtf8<R> {
    pub fn new(inner: R) -> Self { Self::with_chunk(inner, 64 * 1024) }

    pub fn with_chunk(inner: R, chunk: usize) -> Self {
        LossyUtf8 { inner, chunk: chunk.max(1), out: Vec::new(), pos: 0, carry: Vec::new(), eof: false }
    }

    fn fill(&mut self) -> std::io::Result<()> {
        self.out.clear();
        self.pos = 0;
        while self.out.is_empty() && !self.eof {
            let mut data = std::mem::take(&mut self.carry);
            let start = data.len();
            data.resize(start + self.chunk, 0);
            let n = match self.inner.read(&mut data[start..]) {
                Ok(n) => n,
                Err(e) => {
                    // Keep the carried bytes so a retry (e.g. on Interrupted) still sees them.
                    data.truncate(start);
                    self.carry = data;
                    return Err(e);
                }
            };
            data.truncate(start + n);
            if n == 0 { self.eof = true; }
            let mut rest: &[u8] = &data;
            loop {
                match std::str::from_utf8(rest) {
                    Ok(v) => { self.out.extend_from_slice(v.as_bytes()); break; }
                    Err(e) => {
                        let (good, bad) = rest.split_at(e.valid_up_to());
                        self.out.extend_from_slice(good);
                        match e.error_len() {
                            Some(len) => { self.out.extend_from_slice("\u{FFFD}".as_bytes()); rest = &bad[len..]; }
                            // Incomplete sequence at the end: wait for more input, unless there is none.
                            None if self.eof => { self.out.extend_from_slice("\u{FFFD}".as_bytes()); break; }
                            None => { self.carry = bad.to_vec(); break; }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for LossyUtf8<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.out.len() { self.fill()?; }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// ── Shared Strings ──

pub fn read_shared_strings(archive: &mut ZipArchive<std::io::Cursor<Vec<u8>>>) -> Vec<String> {
    let mut strings = Vec::new();
    let file = match archive.by_name("xl/sharedStrings.xml") {
        Ok(f) => f,
        Err(_) => return strings,
    };
    let mut reader = Reader::from_reader(BufReader::new(LossyUtf8::new(file)));
    let mut buf = Vec::new();
    let mut in_si = false;
    let mut current = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == "si" => { in_si = true; current.clear(); }
            Ok(Event::End(e)) if e.name().as_ref() == "si" => { in_si = false; strings.push(current.clone()); }
            Ok(Event::Text(e)) if in_si => push_text(&mut current, &e),
            Ok(Event::GeneralRef(r)) if in_si => push_ref(&mut current, &r),
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    strings
}

// ── Sheet Names ──

pub fn read_sheet_names(archive: &mut ZipArchive<std::io::Cursor<Vec<u8>>>) -> Vec<String> {
    let mut names = Vec::new();
    let file = match archive.by_name("xl/workbook.xml") {
        Ok(f) => f,
        Err(_) => return names,
    };
    let mut reader = Reader::from_reader(BufReader::new(LossyUtf8::new(file)));
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == "sheet" => {
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == "name" {
                        names.push(attr.value.to_string());
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    names
}

// ── Styles / Date Detection ──

pub fn read_styles(archive: &mut ZipArchive<std::io::Cursor<Vec<u8>>>) -> StyleInfo {
    let file = match archive.by_name("xl/styles.xml") {
        Ok(f) => f,
        Err(_) => return StyleInfo { is_date: Vec::new() },
    };
    let mut reader = Reader::from_reader(BufReader::new(LossyUtf8::new(file)));
    let mut buf = Vec::new();

    let builtin_date_ids: std::collections::HashSet<u32> =
        [14,15,16,17,18,19,20,21,22,27,28,29,30,31,32,33,34,35,36,45,46,47]
            .iter().copied().collect();
    let mut custom_date_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut xf_fmt_ids: Vec<u32> = Vec::new();
    let mut in_cell_xfs = false;
    let mut in_num_fmts = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match e.name().as_ref() {
                    "numFmts" => in_num_fmts = true,
                    "cellXfs" => in_cell_xfs = true,
                    _ => process_style_element(e, in_num_fmts, in_cell_xfs, &mut custom_date_ids, &mut xf_fmt_ids),
                }
            }
            Ok(Event::End(ref e)) => {
                match e.name().as_ref() {
                    "numFmts" => in_num_fmts = false,
                    "cellXfs" => in_cell_xfs = false,
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                process_style_element(e, in_num_fmts, in_cell_xfs, &mut custom_date_ids, &mut xf_fmt_ids);
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    let is_date: Vec<bool> = xf_fmt_ids.iter()
        .map(|id| builtin_date_ids.contains(id) || custom_date_ids.contains(id))
        .collect();
    StyleInfo { is_date }
}

fn process_style_element(
    e: &quick_xml::events::BytesStart<'_>,
    in_num_fmts: bool, in_cell_xfs: bool,
    custom_date_ids: &mut std::collections::HashSet<u32>,
    xf_fmt_ids: &mut Vec<u32>,
) {
    if in_num_fmts && e.name().as_ref() == "numFmt" {
        let mut fmt_id: u32 = 0;
        let mut fmt_code = String::new();
        for attr in e.attributes().flatten() {
            match attr.key.as_ref() {
                "numFmtId" => fmt_id = attr.value.parse().unwrap_or(0),
                "formatCode" => fmt_code = attr.value.to_string(),
                _ => {}
            }
        }
        if is_date_format_code(&fmt_code) { custom_date_ids.insert(fmt_id); }
    } else if in_cell_xfs && e.name().as_ref() == "xf" {
        let mut num_fmt_id: u32 = 0;
        for attr in e.attributes().flatten() {
            if attr.key.as_ref() == "numFmtId" {
                num_fmt_id = attr.value.parse().unwrap_or(0);
            }
        }
        xf_fmt_ids.push(num_fmt_id);
    }
}

fn is_date_format_code(code: &str) -> bool {
    let lower = code.to_lowercase();
    let mut cleaned = String::new();
    let mut in_quote = false;
    let mut chars = lower.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' { in_quote = !in_quote; }
        else if ch == '\\' { chars.next(); }
        else if !in_quote { cleaned.push(ch); }
    }
    // A date format must contain at least two of {d, m, y}.
    [
        cleaned.contains('d'),
        cleaned.contains('m'),
        cleaned.contains('y'),
    ].iter().filter(|x| **x).count() >= 2
}

// ── Sheet Parsing ──

pub fn parse_sheet(
    archive: &mut ZipArchive<std::io::Cursor<Vec<u8>>>,
    sheet_idx: usize,
    shared_strings: &[String],
) -> SheetData {
    let path = format!("xl/worksheets/sheet{}.xml", sheet_idx + 1);
    let file = match archive.by_name(&path) {
        Ok(f) => f,
        Err(_) => return SheetData { cells: vec![], merges: vec![], max_row: 0, max_col: 0, rows_with_sum: std::collections::HashSet::new(), cell_map: HashMap::new() },
    };

    let mut reader = Reader::from_reader(BufReader::new(LossyUtf8::new(file)));
    let mut buf = Vec::new();
    let mut cells: Vec<CellData> = Vec::new();
    let mut merges: Vec<RawMerge> = Vec::new();
    let mut max_row = 0usize;
    let mut max_col = 0usize;

    let mut cur_ref = String::new();
    let mut cur_type = String::new();
    let mut cur_style: usize = 0;
    let mut in_value = false;
    let mut in_formula = false;
    let mut in_inline_str = false;
    let mut in_inline_t = false;
    let mut val_text = String::new();
    let mut formula_text = String::new();
    let mut inline_text = String::new();
    let mut cur_shared_si: Option<String> = None;
    let mut cur_shared_is_master = false;

    // Shared formula masters: si → (formula_text, master_row, master_col)
    let mut shared_formulas: HashMap<String, (String, usize, usize)> = HashMap::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                "c" => {
                    cur_ref.clear(); cur_type.clear(); cur_style = 0;
                    val_text.clear(); formula_text.clear(); inline_text.clear();
                    in_inline_str = false; in_inline_t = false;
                    cur_shared_si = None; cur_shared_is_master = false;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            "r" => cur_ref = attr.value.to_string(),
                            "t" => cur_type = attr.value.to_string(),
                            "s" => cur_style = attr.value.parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                "v" => in_value = true,
                "f" => {
                    in_formula = true;
                    // Check for shared formula attributes
                    let mut is_shared = false;
                    let mut has_ref = false;
                    let mut si_val = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            "t" => { if attr.value == "shared" { is_shared = true; } }
                            "si" => { si_val = attr.value.to_string(); }
                            "ref" => { has_ref = true; }
                            _ => {}
                        }
                    }
                    if is_shared && !si_val.is_empty() {
                        cur_shared_si = Some(si_val);
                        cur_shared_is_master = has_ref; // master has both si and ref
                    }
                }
                "is" => in_inline_str = true,
                "t" if in_inline_str => in_inline_t = true,
                _ => {}
            },
            // Handle empty <f t="shared" si="0"/> (reference without text)
            Ok(Event::Empty(ref e)) if e.name().as_ref() == "f" => {
                let mut si_val = String::new();
                let mut is_shared = false;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        "t" => { if attr.value == "shared" { is_shared = true; } }
                        "si" => { si_val = attr.value.to_string(); }
                        _ => {}
                    }
                }
                if is_shared && !si_val.is_empty() {
                    cur_shared_si = Some(si_val);
                    cur_shared_is_master = false;
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                "c" => {
                    if !cur_ref.is_empty() {
                        let (row, col) = parse_cell_ref(&cur_ref);
                        max_row = max_row.max(row + 1);
                        max_col = max_col.max(col + 1);

                        let value = if cur_type == "s" {
                            val_text.parse::<usize>().ok().and_then(|i| shared_strings.get(i)).cloned()
                        } else if cur_type == "inlineStr" && !inline_text.is_empty() {
                            Some(inline_text.clone())
                        } else if !val_text.is_empty() {
                            Some(val_text.clone())
                        } else { None };

                        // Resolve formula: direct text, shared master, or shared reference
                        let formula = if !formula_text.is_empty() {
                            // Store as shared master if applicable
                            if let Some(ref si) = cur_shared_si {
                                if cur_shared_is_master {
                                    shared_formulas.insert(si.clone(), (formula_text.clone(), row, col));
                                }
                            }
                            Some(formula_text.clone())
                        } else if let Some(ref si) = cur_shared_si {
                            // Shared reference — adjust master formula for this cell's position
                            shared_formulas.get(si).map(|(master_formula, master_row, master_col)| {
                                adjust_formula(master_formula, *master_row, *master_col, row, col)
                            })
                        } else {
                            None
                        };

                        if value.is_some() || formula.is_some() {
                            cells.push(CellData { row, col, value, formula, cell_type: cur_type.clone(), style_idx: cur_style });
                        }
                    }
                }
                "v" => in_value = false,
                "f" => in_formula = false,
                "is" => in_inline_str = false,
                "t" if in_inline_str => in_inline_t = false,
                _ => {}
            },
            Ok(Event::Empty(e)) if e.name().as_ref() == "mergeCell" => {
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == "ref" {
                        let s = attr.value.to_string();
                        if let Some(((r1,c1),(r2,c2))) = parse_range_str(&s) {
                            merges.push(RawMerge { min_row: r1, min_col: c1, max_row: r2, max_col: c2 });
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if in_value { push_text(&mut val_text, &e); }
                else if in_formula { push_text(&mut formula_text, &e); }
                else if in_inline_t { push_text(&mut inline_text, &e); }
            }
            Ok(Event::GeneralRef(r)) => {
                if in_value { push_ref(&mut val_text, &r); }
                else if in_formula { push_ref(&mut formula_text, &r); }
                else if in_inline_t { push_ref(&mut inline_text, &r); }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    let mut cell_map = HashMap::with_capacity(cells.len());
    let mut rows_with_sum = std::collections::HashSet::new();
    for (i, cd) in cells.iter().enumerate() {
        cell_map.insert((cd.row, cd.col), i);
        if let Some(ref f) = cd.formula {
            let upper = f.to_uppercase();
            if upper.contains("SUM(") || upper.contains("SUBTOTAL(") {
                rows_with_sum.insert(cd.row);
            }
        }
    }

    SheetData { cells, merges, max_row, max_col, rows_with_sum, cell_map }
}

// ── Value Conversion ──

pub fn excel_serial_to_date(serial: f64) -> Option<String> {
    if !(1.0..=2958465.0).contains(&serial) { return None; }
    let adjusted = if serial >= 60.0 { serial - 1.0 } else { serial };
    let days_since_1900 = adjusted - 1.0;
    let unix_days = days_since_1900 - 25567.0;
    let secs = (unix_days * 86400.0) as i64;
    let days = secs / 86400;
    let mut y = 1970i32;
    let mut remaining = days;
    if remaining >= 0 {
        loop {
            let diy = if is_leap(y) { 366 } else { 365 };
            if remaining < diy { break; }
            remaining -= diy;
            y += 1;
        }
    } else {
        loop {
            y -= 1;
            let diy = if is_leap(y) { 366 } else { 365 };
            remaining += diy;
            if remaining >= 0 { break; }
        }
    }
    let month_days = if is_leap(y) { [31,29,31,30,31,30,31,31,30,31,30,31] } else { [31,28,31,30,31,30,31,31,30,31,30,31] };
    let mut m = 1u32;
    for &md in &month_days {
        if remaining < md { break; }
        remaining -= md;
        m += 1;
    }
    let d = remaining + 1;
    Some(format!("{:04}-{:02}-{:02}", y, m, d))
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn cell_to_json(cd: &CellData, styles: &StyleInfo) -> serde_json::Value {
    let val = cd.value.as_deref();
    let formula = cd.formula.as_deref();

    // Convert value
    let json_val = if let Some(v) = val {
        if styles.is_date_style(cd.style_idx) {
            if let Ok(n) = v.parse::<f64>() {
                if let Some(ds) = excel_serial_to_date(n) {
                    serde_json::Value::String(ds)
                } else {
                    parse_number_or_string(v)
                }
            } else {
                serde_json::Value::String(v.to_string())
            }
        } else {
            parse_number_or_string(v)
        }
    } else {
        serde_json::Value::Null
    };

    // If formula present, return [value, =formula]
    if let Some(f) = formula {
        let formula_str = if f.starts_with('=') { f.to_string() } else { format!("={}", f) };
        serde_json::json!([json_val, formula_str])
    } else {
        json_val
    }
}

/// Adjust a formula's cell references by a row/col offset.
/// e.g., "ROUND(C3*0.1,2)" with row_delta=1 → "ROUND(C4*0.1,2)"
fn adjust_formula(formula: &str, master_row: usize, master_col: usize, target_row: usize, target_col: usize) -> String {
    let row_delta = target_row as i64 - master_row as i64;
    let col_delta = target_col as i64 - master_col as i64;

    // Regex to find cell references like A1, $A1, A$1, $A$1, AB123
    let re = regex::Regex::new(r"(\$?)([A-Z]{1,3})(\$?)(\d+)").unwrap();
    let result = re.replace_all(formula, |caps: &regex::Captures<'_>| {
        let col_abs = &caps[1];
        let col_letters = &caps[2];
        let row_abs = &caps[3];
        let row_num: i64 = caps[4].parse().unwrap_or(0);

        let new_col = if col_abs == "$" {
            col_letters.to_string()
        } else {
            // Decode col letters to number, add delta, encode back
            let mut col_num: i64 = 0;
            for ch in col_letters.chars() {
                col_num = col_num * 26 + (ch as i64 - 'A' as i64 + 1);
            }
            col_num += col_delta;
            if col_num < 1 { col_num = 1; }
            col_num_to_letters(col_num as usize)
        };

        let new_row = if row_abs == "$" {
            row_num
        } else {
            let r = row_num + row_delta;
            if r < 1 { 1 } else { r }
        };

        format!("{}{}{}{}", col_abs, new_col, row_abs, new_row)
    });

    result.to_string()
}

fn col_num_to_letters(mut col: usize) -> String {
    let mut result = String::new();
    while col > 0 {
        col -= 1;
        result.insert(0, (b'A' + (col % 26) as u8) as char);
        col /= 26;
    }
    result
}

fn parse_number_or_string(v: &str) -> serde_json::Value {
    if let Ok(n) = v.parse::<f64>() {
        if n.is_finite() {
            if n.fract() == 0.0 && n.abs() < (i64::MAX as f64) {
                serde_json::Value::Number(serde_json::Number::from(n as i64))
            } else {
                serde_json::Number::from_f64(n)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::String(v.to_string()))
            }
        } else {
            serde_json::Value::String(v.to_string())
        }
    } else {
        serde_json::Value::String(v.to_string())
    }
}
