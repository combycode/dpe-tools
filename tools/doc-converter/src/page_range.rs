//! Page range parsing.
//!
//! Grammar:
//!   "N-M"
//!     N > 0 → start at page N (1-based), apply to M pages
//!     N = 0 → last M pages (if doc has D pages, covers D-M+1 .. D)
//!
//! Examples (for a 10-page document):
//!   "1-1"  → [1]
//!   "1-3"  → [1, 2, 3]
//!   "5-1"  → [5]
//!   "0-3"  → [8, 9, 10]
//!   "0-1"  → [10]
//!
//! Ranges extending past the document are clamped (never panic).
//! Multiple ranges in a slice are unioned and deduplicated.

use std::collections::BTreeSet;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRange {
    /// 0 means "from end". Otherwise 1-based start page.
    pub start: u32,
    /// Count of pages. Must be >= 1.
    pub count: u32,
}

#[derive(Debug, Error, PartialEq)]
pub enum ParseError {
    #[error("range is empty")]
    Empty,
    #[error("range '{0}' missing '-'")]
    MissingDash(String),
    #[error("range '{0}' has non-numeric component")]
    NotNumeric(String),
    #[error("range '{0}' has zero count")]
    ZeroCount(String),
}

impl PageRange {
    /// Parse one range string. Accepts optional leading `p` for symmetry
    /// with filename overlays (e.g. `p1-3.txt` → "1-3" or "p1-3").
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        if s.is_empty() { return Err(ParseError::Empty); }
        let body = s.strip_prefix('p').unwrap_or(s);
        let (n, m) = body.split_once('-').ok_or_else(|| ParseError::MissingDash(s.into()))?;
        let start: u32 = n.parse().map_err(|_| ParseError::NotNumeric(s.into()))?;
        let count: u32 = m.parse().map_err(|_| ParseError::NotNumeric(s.into()))?;
        if count == 0 { return Err(ParseError::ZeroCount(s.into())); }
        Ok(Self { start, count })
    }

    /// Expand this range against a document with `total` pages, yielding
    /// 1-based page numbers, clamped to [1, total].
    pub fn expand(&self, total: u32) -> Vec<u32> {
        if total == 0 { return vec![]; }
        let (first, last) = if self.start == 0 {
            // from-end: last `count` pages
            let first = total.saturating_sub(self.count).saturating_add(1).max(1);
            (first, total)
        } else {
            let first = self.start;
            let last = first.saturating_add(self.count).saturating_sub(1).min(total);
            (first, last)
        };
        if first > total { return vec![]; }
        (first..=last).collect()
    }

    /// Does this range cover page `page` (1-based) in a `total`-page doc?
    pub fn covers(&self, page: u32, total: u32) -> bool {
        self.expand(total).contains(&page)
    }
}

/// Parse a list of range strings (typically from settings.page_filter).
/// Empty input returns an empty Vec.
pub fn parse_many(ranges: &[String]) -> Result<Vec<PageRange>, ParseError> {
    ranges.iter().map(|s| PageRange::parse(s)).collect()
}

/// Union the expansions of many ranges against a `total`-page doc.
/// Returns sorted unique 1-based page numbers.
pub fn union_pages(ranges: &[PageRange], total: u32) -> Vec<u32> {
    let mut set: BTreeSet<u32> = BTreeSet::new();
    for r in ranges {
        for p in r.expand(total) { set.insert(p); }
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn parse_basic() {
        assert_eq!(PageRange::parse("1-1").unwrap(), PageRange { start: 1, count: 1 });
        assert_eq!(PageRange::parse("1-3").unwrap(), PageRange { start: 1, count: 3 });
        assert_eq!(PageRange::parse("5-1").unwrap(), PageRange { start: 5, count: 1 });
        assert_eq!(PageRange::parse("0-3").unwrap(), PageRange { start: 0, count: 3 });
        assert_eq!(PageRange::parse("p1-3").unwrap(), PageRange { start: 1, count: 3 });
    }

    #[test] fn parse_errors() {
        assert_eq!(PageRange::parse(""),     Err(ParseError::Empty));
        assert!(matches!(PageRange::parse("5"),   Err(ParseError::MissingDash(_))));
        assert!(matches!(PageRange::parse("a-b"), Err(ParseError::NotNumeric(_))));
        assert!(matches!(PageRange::parse("2-0"), Err(ParseError::ZeroCount(_))));
    }

    #[test] fn expand_start_at_page() {
        let r = PageRange::parse("1-3").unwrap();
        assert_eq!(r.expand(10), vec![1, 2, 3]);
        assert_eq!(r.expand(2),  vec![1, 2]);       // clamped by total
    }

    #[test] fn expand_last_n() {
        let r = PageRange::parse("0-3").unwrap();
        assert_eq!(r.expand(10), vec![8, 9, 10]);
        assert_eq!(r.expand(2),  vec![1, 2]);
        assert_eq!(r.expand(1),  vec![1]);
        assert_eq!(r.expand(0),  Vec::<u32>::new());
    }

    #[test] fn expand_middle_page() {
        let r = PageRange::parse("5-1").unwrap();
        assert_eq!(r.expand(10), vec![5]);
        assert_eq!(r.expand(3),  Vec::<u32>::new());  // start past total
    }

    #[test] fn expand_overflow_safe() {
        let r = PageRange::parse("8-10").unwrap();
        assert_eq!(r.expand(10), vec![8, 9, 10]);       // clamped
    }

    #[test] fn union_pages_dedupes_and_sorts() {
        let ranges = vec![
            PageRange::parse("1-3").unwrap(),
            PageRange::parse("0-3").unwrap(),
        ];
        assert_eq!(union_pages(&ranges, 10), vec![1, 2, 3, 8, 9, 10]);
    }

    #[test] fn union_pages_overlapping() {
        let ranges = vec![
            PageRange::parse("1-5").unwrap(),
            PageRange::parse("3-4").unwrap(),    // overlaps
        ];
        assert_eq!(union_pages(&ranges, 10), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test] fn covers_inside_and_outside() {
        let r = PageRange::parse("1-3").unwrap();
        assert!(r.covers(1, 10));
        assert!(r.covers(3, 10));
        assert!(!r.covers(4, 10));
    }

    #[test] fn covers_last_n() {
        let r = PageRange::parse("0-2").unwrap();
        assert!(r.covers(9,  10));
        assert!(r.covers(10, 10));
        assert!(!r.covers(8, 10));
    }

    #[test] fn empty_input_is_empty() {
        assert_eq!(union_pages(&[], 10), Vec::<u32>::new());
        assert_eq!(parse_many(&[]).unwrap(), Vec::<PageRange>::new());
    }
}
