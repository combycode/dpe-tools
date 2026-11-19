//! Batch-mode state persistence (Phase 4).
//!
//! When the tool submits a batch to a provider, the batch may take
//! minutes to hours to complete. A re-run that sees the same input
//! document MUST find the in-flight batch instead of submitting a
//! second one — that's what this module stores.
//!
//! Layout: `<state_dir>/<32-hex-of-doc-cache-key>.json`. One file per
//! pending batch. Atomic writes via tmp+rename so a kill mid-write
//! never leaves a half-baked state file. Stale files (provider TTL
//! exceeded) are detected on read by checking `expires_at` against
//! `now`; expired → discarded silently and treated as a fresh submit.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One pending batch's persisted state. The fields here are
/// exhaustive — we don't add anything else without a key version
/// bump (the file's contents are part of the on-disk schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingBatch {
    /// Hex-encoded hash of the doc cache key. Echoes the filename.
    pub doc_cache_key_hash: String,
    /// Provider-issued batch identifier. Anthropic: `msgbatch_...`.
    /// Gemini: `batches/<numeric>`.
    pub batch_id: String,
    /// Which provider the batch was submitted to. Used as a sanity
    /// gate — refuse to poll an Anthropic batch_id with a Google-
    /// configured provider.
    pub provider: String,
    /// Source PDF path, for logs/debug.
    pub source_path: String,
    /// Number of pages in the batch.
    pub page_count: u32,
    /// Wall-clock submit time (millis since epoch).
    pub submitted_at_ms: u64,
    /// Provider TTL — when the batch will be discarded upstream.
    /// Anthropic: +24h, Gemini: +48h. Past this, we throw away the
    /// state file and treat as a fresh submit.
    pub expires_at_ms: u64,
    /// Mirror of `cfg.model` at submission. Future poll runs that
    /// have a different model in their settings should NOT match
    /// (the doc_cache_key already covers that, but we also assert
    /// it here as a belt-and-suspenders check).
    pub model: String,
    /// Submission-order custom_id list. Position N here was page
    /// `page_map[N]` in the source PDF. Used to correlate batch
    /// results back to original pages when the provider returns
    /// position-based responses (Gemini inline mode).
    pub custom_ids: Vec<String>,
}

/// Path to the state file for a given doc cache-key hash.
pub fn state_path(state_dir: &Path, doc_cache_key_hash: &str) -> PathBuf {
    state_dir.join(format!("{doc_cache_key_hash}.json"))
}

/// Read a pending batch by cache-key hash. Returns:
///   - `Ok(Some(_))` — file existed, parsed, NOT expired.
///   - `Ok(None)` — file absent, OR file present but expired
///     (in which case we delete it as a side effect).
///   - `Err(_)` — IO or parse error on a present, non-expired file.
pub fn read_pending(
    state_dir: &Path,
    doc_cache_key_hash: &str,
    now_ms: u64,
) -> std::io::Result<Option<PendingBatch>> {
    let p = state_path(state_dir, doc_cache_key_hash);
    if !p.is_file() { return Ok(None); }
    let raw = std::fs::read_to_string(&p)?;
    let pending: PendingBatch = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            // Corrupted file: log + ignore. Treat as fresh submit.
            eprintln!("[batch_state] ignoring unparseable {}: {}", p.display(), e);
            let _ = std::fs::remove_file(&p);
            return Ok(None);
        }
    };
    if now_ms >= pending.expires_at_ms {
        // Stale: provider has discarded the batch upstream. Remove
        // the file and treat as fresh.
        let _ = std::fs::remove_file(&p);
        return Ok(None);
    }
    Ok(Some(pending))
}

/// Atomically persist a pending batch. Writes to `<path>.tmp` then
/// renames over `<path>` so a kill mid-write never produces a
/// torn file.
pub fn persist_pending(
    state_dir: &Path,
    pending: &PendingBatch,
) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let final_path = state_path(state_dir, &pending.doc_cache_key_hash);
    let tmp_path   = final_path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(pending)
        .map_err(std::io::Error::other)?;
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Remove a pending state file (called when the batch completes
/// successfully OR fails terminally — both clear the slot so a
/// future re-run starts fresh).
pub fn clear_pending(state_dir: &Path, doc_cache_key_hash: &str) -> std::io::Result<()> {
    let p = state_path(state_dir, doc_cache_key_hash);
    match std::fs::remove_file(&p) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn mk_pending(hash: &str, expires: u64) -> PendingBatch {
        PendingBatch {
            doc_cache_key_hash: hash.to_string(),
            batch_id: "msgbatch_01HX".to_string(),
            provider: "anthropic".to_string(),
            source_path: "D:/x/y.pdf".to_string(),
            page_count: 14,
            submitted_at_ms: 1_000_000,
            expires_at_ms: expires,
            model: "claude-sonnet-4-6".to_string(),
            custom_ids: vec!["page-1".into(), "page-2".into()],
        }
    }

    #[test]
    fn round_trip_persist_then_read() {
        let d = tempdir().unwrap();
        let p = mk_pending("aaa", 9_999_999_999);
        persist_pending(d.path(), &p).unwrap();
        let back = read_pending(d.path(), "aaa", 1_000_000).unwrap().unwrap();
        assert_eq!(back.batch_id, p.batch_id);
        assert_eq!(back.page_count, p.page_count);
        assert_eq!(back.custom_ids, p.custom_ids);
    }

    #[test]
    fn read_pending_missing_returns_none() {
        let d = tempdir().unwrap();
        assert!(read_pending(d.path(), "nope", 1_000_000).unwrap().is_none());
    }

    #[test]
    fn read_pending_expired_returns_none_and_deletes_file() {
        let d = tempdir().unwrap();
        // expires_at_ms = 100; now = 200 → expired.
        let p = mk_pending("expired-hash", 100);
        persist_pending(d.path(), &p).unwrap();
        // Confirm file wrote.
        assert!(state_path(d.path(), "expired-hash").is_file());
        // Read with now > expires → None + file removed.
        let r = read_pending(d.path(), "expired-hash", 200).unwrap();
        assert!(r.is_none());
        assert!(!state_path(d.path(), "expired-hash").is_file(),
            "expired file must be cleaned up on read");
    }

    #[test]
    fn read_pending_corrupt_file_returns_none_and_deletes() {
        let d = tempdir().unwrap();
        let p = state_path(d.path(), "corrupt");
        std::fs::write(&p, "not-json{{{").unwrap();
        let r = read_pending(d.path(), "corrupt", 1_000_000).unwrap();
        assert!(r.is_none());
        assert!(!p.is_file(), "corrupt file must be cleaned up");
    }

    #[test]
    fn clear_pending_idempotent() {
        let d = tempdir().unwrap();
        // Clearing absent file is OK.
        clear_pending(d.path(), "absent").unwrap();
        // Clearing present file removes it.
        let p = mk_pending("present", 9_999_999_999);
        persist_pending(d.path(), &p).unwrap();
        assert!(state_path(d.path(), "present").is_file());
        clear_pending(d.path(), "present").unwrap();
        assert!(!state_path(d.path(), "present").is_file());
    }

    #[test]
    fn persist_creates_parent_dir_atomically() {
        let d = tempdir().unwrap();
        let nested = d.path().join("a/b/c");
        // Parent doesn't exist yet; persist must mkdir -p.
        let p = mk_pending("nested", 9_999_999_999);
        persist_pending(&nested, &p).unwrap();
        assert!(state_path(&nested, "nested").is_file());
        // No tmp file left behind.
        assert!(!state_path(&nested, "nested").with_extension("json.tmp").is_file());
    }
}
