//! Prompt composition for the doc converter.
//!
//! Files live flat in `prompts.dir` with names `<name>_<version>[_<overlay>].txt`.
//!
//! For each page, the final system prompt is:
//!
//!   base
//!   + tag overlays         (one per name in settings.overlays; value resolved
//!     from envelope.v[name] or settings.overlay_defaults)
//!   + page-range overlays  (every `<name>_<version>_pN-M.txt` covering this page)
//!
//! Tag overlays append in the order of `settings.overlays`. Page-range
//! overlays append in filename-sort order (deterministic). Missing files
//! are skipped silently; missing base is a hard error.
//!
//! Sections are joined with `\n\n---\n\n`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::page_range::{PageRange, ParseError};

const SEPARATOR: &str = "\n\n---\n\n";

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("base prompt not found: {0}")]
    BaseMissing(PathBuf),
    #[error("read prompt '{path}': {reason}")]
    Read { path: PathBuf, reason: String },
    #[error("bad page-range overlay filename '{0}'")]
    BadOverlay(String),
}

#[derive(Debug, Clone)]
pub struct LoadInput<'a> {
    pub dir:      &'a Path,
    pub name:     &'a str,
    pub version:  &'a str,
    /// Ordered tag-overlay pairs: (overlay_name, resolved_value). Empty value = skip.
    pub tags:     &'a [(String, String)],
    /// 1-based page number within the source document.
    pub page:     u32,
    /// Total pages in the document.
    pub total:   u32,
}

pub fn base_filename(name: &str, version: &str) -> String {
    format!("{}_{}.txt", name, version)
}

pub fn overlay_filename(name: &str, version: &str, overlay: &str) -> String {
    format!("{}_{}_{}.txt", name, version, overlay)
}

pub fn page_overlay_filename(name: &str, version: &str, r: &PageRange) -> String {
    format!("{}_{}_p{}-{}.txt", name, version, r.start, r.count)
}

/// Compose the prompt for a single page. Always succeeds as long as base
/// exists; missing overlays are silently skipped.
pub fn compose(input: &LoadInput<'_>) -> Result<String, PromptError> {
    let base_path = input.dir.join(base_filename(input.name, input.version));
    if !base_path.is_file() {
        return Err(PromptError::BaseMissing(base_path));
    }
    let mut parts: Vec<String> = Vec::new();
    parts.push(read_trim(&base_path)?);

    // 1) Tag overlays in declared order.
    for (_tag_name, value) in input.tags {
        if value.is_empty() { continue; }
        let p = input.dir.join(overlay_filename(input.name, input.version, value));
        if p.is_file() {
            let t = read_trim(&p)?;
            if !t.is_empty() { parts.push(t); }
        }
    }

    // 2) Page-range overlays: scan dir for matching files, sort by filename,
    //    include those whose range covers this page.
    for (range_file_name, range) in discover_page_overlays(input.dir, input.name, input.version)? {
        if range.covers(input.page, input.total) {
            let p = input.dir.join(&range_file_name);
            let t = read_trim(&p)?;
            if !t.is_empty() { parts.push(t); }
        }
    }

    Ok(parts.join(SEPARATOR))
}

/// Compose the prompt for whole-doc mode (Phase 3). Different from
/// `compose()`: the LLM gets the WHOLE PDF as one attachment, not a
/// per-page render. Two semantic differences from per-page:
///
///   1. Page-range overlays are SKIPPED entirely. They're per-page
///      instructions ("apply this on the cover page") that don't
///      compose meaningfully when the doc is processed as one unit.
///      If a pipeline wants page-1-special handling in whole-doc mode,
///      it goes in (2).
///   2. A dedicated `<name>_<version>_whole.txt` overlay is loaded
///      AFTER tag overlays (if present). This is the explicit place
///      to put whole-doc-specific instructions ("scan the entire
///      document and produce a unified summary"). Missing → skipped
///      silently (whole-doc mode works without one).
///
/// Output prompt structure:
///
/// ```text
/// <base>
/// ---
/// <tag overlays in settings.overlays order>
/// ---
/// <whole-doc overlay if present>
/// ```
///
/// The same base + tag-overlay rules as `compose()` apply.
pub fn compose_whole_doc(input: &WholeDocInput<'_>) -> Result<String, PromptError> {
    let base_path = input.dir.join(base_filename(input.name, input.version));
    if !base_path.is_file() {
        return Err(PromptError::BaseMissing(base_path));
    }
    let mut parts: Vec<String> = Vec::new();
    parts.push(read_trim(&base_path)?);

    // 1) Tag overlays — same resolution as per-page.
    for (_tag_name, value) in input.tags {
        if value.is_empty() { continue; }
        let p = input.dir.join(overlay_filename(input.name, input.version, value));
        if p.is_file() {
            let t = read_trim(&p)?;
            if !t.is_empty() { parts.push(t); }
        }
    }

    // 2) Whole-doc overlay — `<name>_<version>_whole.txt`. Optional.
    let whole_path = input.dir.join(overlay_filename(input.name, input.version, "whole"));
    if whole_path.is_file() {
        let t = read_trim(&whole_path)?;
        if !t.is_empty() { parts.push(t); }
    }

    // (No page-range overlays — the WHOLE doc is one unit.)

    Ok(parts.join(SEPARATOR))
}

/// Inputs for whole-doc prompt composition. Mirrors `LoadInput` but
/// without `page` / `total` — whole-doc mode has no per-page context.
#[derive(Debug, Clone)]
pub struct WholeDocInput<'a> {
    pub dir:      &'a Path,
    pub name:     &'a str,
    pub version:  &'a str,
    /// Ordered tag-overlay pairs: (overlay_name, resolved_value). Empty value = skip.
    pub tags:     &'a [(String, String)],
}

fn read_trim(p: &Path) -> Result<String, PromptError> {
    fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .map_err(|e| PromptError::Read { path: p.to_path_buf(), reason: e.to_string() })
}

/// Scan `dir` for `<name>_<version>_p<N>-<M>.txt` files, sorted by filename.
/// Returns (filename, parsed range) pairs. Non-matching files ignored.
pub fn discover_page_overlays(
    dir: &Path,
    name: &str,
    version: &str,
) -> Result<Vec<(String, PageRange)>, PromptError> {
    let prefix = format!("{}_{}_p", name, version);
    let suffix = ".txt";
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(vec![]),
    };
    let mut collected: BTreeMap<String, PageRange> = BTreeMap::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let Some(middle) = file_name.strip_prefix(&prefix) else { continue };
        let Some(range_str) = middle.strip_suffix(suffix) else { continue };
        match PageRange::parse(range_str) {
            Ok(r) => { collected.insert(file_name, r); }
            Err(ParseError::Empty) | Err(ParseError::MissingDash(_)) => continue,
            Err(_) => return Err(PromptError::BadOverlay(file_name)),
        }
    }
    Ok(collected.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};

    fn setup() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        create_dir_all(d.path()).unwrap();
        d
    }

    fn t(path: &Path, body: &str) { write(path, body).unwrap(); }

    fn empty_tags() -> Vec<(String, String)> { vec![] }

    #[test]
    fn base_only() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        let tags = empty_tags();
        let p = compose(&LoadInput {
            dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 1,
        }).unwrap();
        assert_eq!(p, "BASE");
    }

    #[test]
    fn base_plus_tags_in_order() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_alpha.txt"), "OVERLAY_A");
        t(&d.path().join("convert_v1_bravo.txt"),  "OVERLAY_B");
        let tags = vec![("category".into(), "alpha".into()), ("kind".into(), "bravo".into())];
        let p = compose(&LoadInput {
            dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 1,
        }).unwrap();
        assert_eq!(p, "BASE\n\n---\n\nOVERLAY_A\n\n---\n\nOVERLAY_B");
    }

    #[test]
    fn missing_overlays_silently_skipped() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_bravo.txt"), "OVERLAY_B");
        let tags = vec![("category".into(), "alpha".into()), ("kind".into(), "bravo".into())];
        let p = compose(&LoadInput {
            dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 1,
        }).unwrap();
        assert_eq!(p, "BASE\n\n---\n\nOVERLAY_B");
    }

    #[test]
    fn base_missing_is_hard_error() {
        let d = setup();
        let tags = empty_tags();
        let err = compose(&LoadInput {
            dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 1,
        }).unwrap_err();
        assert!(matches!(err, PromptError::BaseMissing(_)));
    }

    #[test]
    fn page_overlay_first_page() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_p1-1.txt"), "PAGE1");
        let tags = empty_tags();
        let p1 = compose(&LoadInput { dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 10 }).unwrap();
        let p2 = compose(&LoadInput { dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 2, total: 10 }).unwrap();
        assert_eq!(p1, "BASE\n\n---\n\nPAGE1");
        assert_eq!(p2, "BASE");                       // page-1 overlay doesn't apply to page 2
    }

    #[test]
    fn page_overlay_last_pages() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_p0-2.txt"), "LAST2");
        let tags = empty_tags();
        let p9 = compose(&LoadInput { dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 9, total: 10 }).unwrap();
        let p10 = compose(&LoadInput { dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 10, total: 10 }).unwrap();
        let p8 = compose(&LoadInput { dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 8, total: 10 }).unwrap();
        assert_eq!(p9, "BASE\n\n---\n\nLAST2");
        assert_eq!(p10, "BASE\n\n---\n\nLAST2");
        assert_eq!(p8, "BASE");
    }

    #[test]
    fn multiple_page_overlays_filename_sorted() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_p1-1.txt"),  "FIRST");
        t(&d.path().join("convert_v1_p1-10.txt"), "FIRST_TEN");
        let tags = empty_tags();
        let p = compose(&LoadInput {
            dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 20,
        }).unwrap();
        // filename sort: "p1-1" < "p1-10" alphabetically
        assert_eq!(p, "BASE\n\n---\n\nFIRST\n\n---\n\nFIRST_TEN");
    }

    #[test]
    fn full_composition_base_tag_page_overlays() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_alpha.txt"), "OVERLAY_A");
        t(&d.path().join("convert_v1_p1-1.txt"), "PAGE1");
        t(&d.path().join("convert_v1_p0-1.txt"), "LAST");
        let tags = vec![("category".into(), "alpha".into())];
        // page 1, total 3: PAGE1 and p0-1 (last-page=3) both apply? no, p0-1 on total=3 → page 3.
        let p1 = compose(&LoadInput {
            dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 3,
        }).unwrap();
        assert_eq!(p1, "BASE\n\n---\n\nOVERLAY_A\n\n---\n\nPAGE1");
    }

    #[test]
    fn filename_builders() {
        assert_eq!(base_filename("convert", "v1"), "convert_v1.txt");
        assert_eq!(overlay_filename("convert", "v1", "alpha"), "convert_v1_alpha.txt");
        let r = PageRange::parse("1-3").unwrap();
        assert_eq!(page_overlay_filename("convert", "v1", &r), "convert_v1_p1-3.txt");
    }

    #[test]
    fn discover_page_overlays_scans_correctly() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"),           "B");
        t(&d.path().join("convert_v1_alpha.txt"),     "T");         // tag overlay, not page
        t(&d.path().join("convert_v1_p1-1.txt"),      "P1");
        t(&d.path().join("convert_v1_p0-3.txt"),      "L3");
        t(&d.path().join("convert_v2_p1-1.txt"),      "V2");         // wrong version
        t(&d.path().join("other_v1_p1-1.txt"),        "X");          // wrong name
        let found = discover_page_overlays(d.path(), "convert", "v1").unwrap();
        let names: Vec<_> = found.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, vec!["convert_v1_p0-3.txt".to_string(), "convert_v1_p1-1.txt".to_string()]);
    }

    #[test]
    fn empty_overlays_skipped() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        let tags = vec![("category".into(), "".into()), ("kind".into(), "".into())];
        let p = compose(&LoadInput {
            dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 1,
        }).unwrap();
        assert_eq!(p, "BASE");
    }

    /// `discover_page_overlays` returns `BadOverlay` when a filename matches
    /// the `<name>_<version>_pXXX.txt` shape but the range part fails to
    /// parse with anything other than `Empty`/`MissingDash` (which are
    /// silently treated as not-an-overlay). A non-numeric range character
    /// triggers a `Number` parse error, which surfaces as `BadOverlay`.
    #[test]
    fn discover_page_overlays_bad_range_errors() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        // `pabc-1` — starts with `p`, has the `-`, but the start segment
        // isn't parseable as a u32. PageRange::parse returns Number(...)
        // → BadOverlay. (Empty / MissingDash skip silently; anything else
        // is a hard error.)
        t(&d.path().join("convert_v1_pabc-1.txt"), "BAD");
        let err = discover_page_overlays(d.path(), "convert", "v1").unwrap_err();
        match err {
            PromptError::BadOverlay(name) => assert_eq!(name, "convert_v1_pabc-1.txt"),
            other => panic!("expected BadOverlay, got {other:?}"),
        }
    }

    /// `read_trim` (used by both base and overlay reads) wraps any std::io
    /// error in `PromptError::Read`. The tag-overlay branch guards with
    /// `is_file()` so a directory there is silently skipped — but the
    /// page-overlay branch (line ~88) does NOT guard, it trusts that
    /// `discover_page_overlays` returned valid entries. `fs::read_dir`
    /// returns directories too, so a directory named like a page overlay
    /// passes discovery, then explodes at `read_to_string` and surfaces
    /// the io error as `PromptError::Read`.
    #[test]
    fn overlay_read_failure_surfaces_as_read_error() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        // A DIRECTORY whose name matches the page-overlay pattern.
        // discover_page_overlays returns it; compose then read_trim's it
        // → io error → PromptError::Read.
        create_dir_all(d.path().join("convert_v1_p1-1.txt")).unwrap();
        let tags = empty_tags();
        let err = compose(&LoadInput {
            dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 1,
        }).unwrap_err();
        match err {
            PromptError::Read { path, .. } => {
                assert!(path.ends_with("convert_v1_p1-1.txt"), "unexpected path {path:?}");
            }
            other => panic!("expected Read, got {other:?}"),
        }
    }

    /// Overlay files whose content is empty or whitespace-only are skipped
    /// by both the tag-overlay branch (line ~78) and the page-overlay
    /// branch (line ~88). Without this the joined prompt would acquire a
    /// dangling `\n\n---\n\n` separator with nothing after it.
    #[test]
    fn whitespace_only_overlay_files_skipped() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_alpha.txt"), "   \n\t  \n");   // tag overlay: whitespace only
        t(&d.path().join("convert_v1_p1-1.txt"), "");                // page overlay: empty
        let tags = vec![("category".into(), "alpha".into())];
        let p = compose(&LoadInput {
            dir: d.path(), name: "convert", version: "v1",
            tags: &tags, page: 1, total: 1,
        }).unwrap();
        // Both overlays trim to empty and are dropped from the join.
        assert_eq!(p, "BASE");
    }

    // ─── Phase 3 / compose_whole_doc (A+D variant) ───────────────────
    //
    // Whole-doc mode loads:
    //   1) base
    //   2) tag overlays (same as per-page)
    //   3) `<name>_<version>_whole.txt` if present (NEW — D variant)
    //   4) NO page-range overlays (A variant)

    #[test]
    fn whole_doc_base_only() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        let tags = empty_tags();
        let p = compose_whole_doc(&WholeDocInput {
            dir: d.path(), name: "convert", version: "v1", tags: &tags,
        }).unwrap();
        assert_eq!(p, "BASE");
    }

    #[test]
    fn whole_doc_includes_tag_overlays() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_alpha.txt"), "ALPHA");
        let tags = vec![("category".into(), "alpha".into())];
        let p = compose_whole_doc(&WholeDocInput {
            dir: d.path(), name: "convert", version: "v1", tags: &tags,
        }).unwrap();
        assert_eq!(p, "BASE\n\n---\n\nALPHA");
    }

    #[test]
    fn whole_doc_loads_dedicated_whole_overlay_when_present() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_whole.txt"), "WHOLE-DOC-INSTRUCTIONS");
        let tags = empty_tags();
        let p = compose_whole_doc(&WholeDocInput {
            dir: d.path(), name: "convert", version: "v1", tags: &tags,
        }).unwrap();
        assert_eq!(p, "BASE\n\n---\n\nWHOLE-DOC-INSTRUCTIONS",
            "the _whole.txt overlay must append after base");
    }

    #[test]
    fn whole_doc_overlay_after_tag_overlays() {
        // Order must be: base, then tag overlays, then whole-doc.
        // This matters because `<name>_<version>_whole.txt` is the
        // place to put "always do X for whole-doc mode" rules that
        // SHOULD apply on top of category-specific instructions.
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_alpha.txt"), "ALPHA");
        t(&d.path().join("convert_v1_whole.txt"), "WHOLE");
        let tags = vec![("category".into(), "alpha".into())];
        let p = compose_whole_doc(&WholeDocInput {
            dir: d.path(), name: "convert", version: "v1", tags: &tags,
        }).unwrap();
        assert_eq!(p, "BASE\n\n---\n\nALPHA\n\n---\n\nWHOLE");
    }

    #[test]
    fn whole_doc_skips_page_range_overlays() {
        // The DEFINING property of A: page-range overlays (p1-N etc.)
        // are NEVER loaded in whole-doc mode, even if they exist on
        // disk. They're per-page rules that don't compose with a
        // single whole-document call.
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_p1-1.txt"),  "FIRST_PAGE");
        t(&d.path().join("convert_v1_p1-3.txt"),  "FIRST_THREE");
        t(&d.path().join("convert_v1_p0-2.txt"),  "LAST_TWO");
        let tags = empty_tags();
        let p = compose_whole_doc(&WholeDocInput {
            dir: d.path(), name: "convert", version: "v1", tags: &tags,
        }).unwrap();
        // Page overlays are present on disk but MUST NOT appear here.
        assert_eq!(p, "BASE",
            "page-range overlays must be skipped in whole-doc mode; got {p:?}");
        assert!(!p.contains("FIRST_PAGE"));
        assert!(!p.contains("FIRST_THREE"));
        assert!(!p.contains("LAST_TWO"));
    }

    #[test]
    fn whole_doc_missing_whole_overlay_silently_skipped() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        // No _whole.txt; no error, just absent from the joined prompt.
        let tags = empty_tags();
        let p = compose_whole_doc(&WholeDocInput {
            dir: d.path(), name: "convert", version: "v1", tags: &tags,
        }).unwrap();
        assert_eq!(p, "BASE");
    }

    #[test]
    fn whole_doc_base_missing_is_hard_error() {
        let d = setup();
        let tags = empty_tags();
        let err = compose_whole_doc(&WholeDocInput {
            dir: d.path(), name: "convert", version: "v1", tags: &tags,
        }).unwrap_err();
        assert!(matches!(err, PromptError::BaseMissing(_)));
    }

    #[test]
    fn whole_doc_whitespace_only_whole_overlay_skipped() {
        let d = setup();
        t(&d.path().join("convert_v1.txt"), "BASE");
        t(&d.path().join("convert_v1_whole.txt"), "   \n  \n");
        let tags = empty_tags();
        let p = compose_whole_doc(&WholeDocInput {
            dir: d.path(), name: "convert", version: "v1", tags: &tags,
        }).unwrap();
        assert_eq!(p, "BASE", "whitespace-only _whole.txt drops out of join");
    }
}
