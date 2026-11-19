//! DOCX → PDF conversion via LibreOffice headless subprocess.
//!
//! Runtime dependency: `soffice` on PATH (or explicit path in settings).
//!
//! Invocation:
//!   soffice -env:UserInstallation=file:///<tmp> --headless \
//!           --convert-to pdf --outdir <out_dir> <input.docx>
//!
//! Each call gets an isolated per-invocation UserInstallation profile. Without
//! it, concurrent soffice processes contend for the default profile lock at
//! `%APPDATA%\LibreOffice\4\user\` (Windows) / `~/.config/libreoffice/4/user/`
//! (Linux) and the losers silently exit 1 with EMPTY stderr — the exact
//! failure mode seen with parallel conversions. `TempDir` cleans the profile up
//! on Drop so nothing accumulates across runs.
//!
//! LibreOffice writes `<input_stem>.pdf` in `out_dir`. We pick it up + return its path.
//!
//! Testable: `convert_via_runner()` takes an injected runner function, so
//! unit tests can mock subprocess behavior without actually invoking soffice.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DocxError {
    #[error("soffice not found or not runnable at '{bin}': {source}")]
    Spawn { bin: String, source: io::Error },
    #[error("soffice exited with code {code}: {stderr}")]
    NonZero { code: i32, stderr: String },
    #[error("converted pdf not found at '{0}' after soffice run")]
    OutputMissing(PathBuf),
    #[error("input file not found: {0}")]
    InputMissing(PathBuf),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Inject-able subprocess runner for tests.
pub type Runner = dyn Fn(&str, &[&str]) -> io::Result<Output>;

pub fn default_runner() -> Box<Runner> {
    Box::new(|bin: &str, args: &[&str]| Command::new(bin).args(args).output())
}

pub fn convert_to_pdf(
    soffice_bin: &str,
    docx_path: &Path,
    out_dir: &Path,
) -> Result<PathBuf, DocxError> {
    convert_via_runner(soffice_bin, docx_path, out_dir, &default_runner())
}

/// Core conversion with injected runner. Returns path to the produced PDF.
pub fn convert_via_runner(
    soffice_bin: &str,
    docx_path: &Path,
    out_dir: &Path,
    runner: &Runner,
) -> Result<PathBuf, DocxError> {
    if !docx_path.is_file() {
        return Err(DocxError::InputMissing(docx_path.to_path_buf()));
    }
    std::fs::create_dir_all(out_dir)?;

    // Isolated profile — held by TempDir across the runner call so its Drop
    // (which deletes the dir) fires only AFTER soffice exits. Any earlier
    // drop would race the child process.
    let profile = TempDir::new()?;
    let profile_uri = format!(
        "-env:UserInstallation=file:///{}",
        profile.path().to_string_lossy().replace('\\', "/"),
    );

    let args = [
        // MUST precede `--headless` — soffice parses `-env:...` before
        // switching to the requested run mode.
        profile_uri.as_str(),
        "--headless",
        "--convert-to", "pdf",
        "--outdir", out_dir.to_str().unwrap_or(""),
        docx_path.to_str().unwrap_or(""),
    ];
    let out = runner(soffice_bin, &args)
        .map_err(|e| DocxError::Spawn { bin: soffice_bin.into(), source: e })?;
    if !out.status.success() {
        return Err(DocxError::NonZero {
            code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        });
    }
    // soffice emits <stem>.pdf in out_dir
    let stem = docx_path.file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| DocxError::OutputMissing(out_dir.join("?.pdf")))?;
    let produced = out_dir.join(format!("{}.pdf", stem));
    if !produced.is_file() {
        return Err(DocxError::OutputMissing(produced));
    }
    Ok(produced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::c_int;
    use std::process::ExitStatus;

    fn exit_status_success() -> ExitStatus {
        #[cfg(windows)]  { use std::os::windows::process::ExitStatusExt; ExitStatus::from_raw(0) }
        #[cfg(unix)]     { use std::os::unix::process::ExitStatusExt;    ExitStatus::from_raw(0) }
    }
    fn exit_status_fail(code: c_int) -> ExitStatus {
        #[cfg(windows)]  { use std::os::windows::process::ExitStatusExt; ExitStatus::from_raw(code as u32) }
        #[cfg(unix)]     { use std::os::unix::process::ExitStatusExt;    ExitStatus::from_raw(code << 8) }
    }

    fn ok_output() -> Output {
        Output { status: exit_status_success(), stdout: vec![], stderr: vec![] }
    }
    fn fail_output(code: c_int, msg: &str) -> Output {
        Output { status: exit_status_fail(code), stdout: vec![], stderr: msg.as_bytes().to_vec() }
    }

    #[test]
    fn successful_conversion_returns_produced_pdf_path() {
        let tmp = tempfile::tempdir().unwrap();
        let docx = tmp.path().join("doc.docx");
        std::fs::write(&docx, b"fake docx").unwrap();
        let out_dir = tmp.path().join("out");

        // Simulated runner: pretend soffice ran and wrote the PDF.
        let produced_pdf = out_dir.join("doc.pdf");
        let produced_pdf_for_runner = produced_pdf.clone();
        let runner: Box<Runner> = Box::new(move |_bin: &str, args: &[&str]| {
            // args[0] is the -env:UserInstallation=file:///<tempdir>; the
            // rest of the invocation shape is asserted in a dedicated test.
            assert!(args[0].starts_with("-env:UserInstallation=file:///"),
                    "expected profile arg first, got '{}'", args[0]);
            assert_eq!(args[1], "--headless");
            assert_eq!(args[2], "--convert-to");
            assert_eq!(args[3], "pdf");
            std::fs::create_dir_all(produced_pdf_for_runner.parent().unwrap()).unwrap();
            std::fs::write(&produced_pdf_for_runner, b"%PDF-1.4\n%%EOF\n").unwrap();
            Ok(ok_output())
        });
        let got = convert_via_runner("soffice", &docx, &out_dir, &runner).unwrap();
        assert_eq!(got, produced_pdf);
    }

    #[test]
    fn profile_arg_precedes_headless_and_points_at_existing_dir() {
        // Regression guard: parallel conversions crashed with `exit 1, empty
        // stderr` because concurrent soffice processes shared one user
        // profile lock. The fix is a per-invocation UserInstallation dir
        // — that arg MUST come before `--headless` (soffice parses -env:
        // pre-mode-switch) and MUST point at a dir that exists on disk.
        let tmp = tempfile::tempdir().unwrap();
        let docx = tmp.path().join("doc.docx");
        std::fs::write(&docx, b"fake docx").unwrap();
        let out_dir = tmp.path().join("out");
        let produced_pdf = out_dir.join("doc.pdf");
        let produced_pdf_for_runner = produced_pdf.clone();
        let runner: Box<Runner> = Box::new(move |_bin: &str, args: &[&str]| {
            let profile = args[0]
                .strip_prefix("-env:UserInstallation=file:///")
                .expect("first arg must be -env:UserInstallation=file:///...");
            // Windows form: strip_prefix yields `C:/.../tmpXYZ` — must be
            // a real dir the OS created for us.
            let profile_native = profile.replace('/', std::path::MAIN_SEPARATOR_STR);
            assert!(
                std::path::Path::new(&profile_native).is_dir(),
                "profile dir must exist while soffice is running: {}",
                profile_native,
            );
            assert_eq!(args[1], "--headless", "-env:... must precede --headless");
            std::fs::create_dir_all(produced_pdf_for_runner.parent().unwrap()).unwrap();
            std::fs::write(&produced_pdf_for_runner, b"%PDF-1.4\n%%EOF\n").unwrap();
            Ok(ok_output())
        });
        convert_via_runner("soffice", &docx, &out_dir, &runner).unwrap();
    }

    #[test]
    fn missing_input_errors_before_running() {
        let tmp = tempfile::tempdir().unwrap();
        let runner: Box<Runner> = Box::new(|_, _| panic!("must not be called"));
        let err = convert_via_runner("soffice", &tmp.path().join("missing.docx"),
                                      tmp.path(), &runner).unwrap_err();
        assert!(matches!(err, DocxError::InputMissing(_)));
    }

    #[test]
    fn nonzero_exit_captured_with_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let docx = tmp.path().join("d.docx");
        std::fs::write(&docx, b"x").unwrap();
        let runner: Box<Runner> = Box::new(|_, _| Ok(fail_output(2, "bad input")));
        let err = convert_via_runner("soffice", &docx, &tmp.path().join("out"), &runner).unwrap_err();
        match err {
            DocxError::NonZero { code, stderr } => {
                assert_eq!(code, 2);
                assert!(stderr.contains("bad input"));
            }
            other => panic!("wrong variant: {:?}", other),
        }
    }

    #[test]
    fn missing_output_after_success_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let docx = tmp.path().join("d.docx");
        std::fs::write(&docx, b"x").unwrap();
        // runner returns success but doesn't actually write the pdf
        let runner: Box<Runner> = Box::new(|_, _| Ok(ok_output()));
        let err = convert_via_runner("soffice", &docx, &tmp.path().join("out"), &runner).unwrap_err();
        assert!(matches!(err, DocxError::OutputMissing(_)));
    }

    #[test]
    fn spawn_error_wrapped() {
        let tmp = tempfile::tempdir().unwrap();
        let docx = tmp.path().join("d.docx");
        std::fs::write(&docx, b"x").unwrap();
        let runner: Box<Runner> = Box::new(|_, _| Err(io::Error::new(io::ErrorKind::NotFound, "exe missing")));
        let err = convert_via_runner("soffice-nope", &docx, &tmp.path().join("out"), &runner).unwrap_err();
        match err {
            DocxError::Spawn { bin, source } => {
                assert_eq!(bin, "soffice-nope");
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("wrong variant: {:?}", other),
        }
    }
}
