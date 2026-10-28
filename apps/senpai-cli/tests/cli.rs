//! End-to-end tests for the CLI, including the benchmark harness.
//!
//! The harness exists to be re-run and compared across commits, so the
//! thing worth testing is that its machine-readable output keeps its
//! shape -- a silently renamed or dropped field makes every previously
//! recorded run incomparable.

use std::path::PathBuf;
use std::process::Command;

/// A one-page, blank PDF with a correct xref table. Smaller than the
/// fixture in `senpai-pdf-poppler`'s tests because nothing here needs
/// text or fonts -- only that the document opens and renders.
fn blank_pdf() -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>",
    ];

    let mut out: Vec<u8> = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn fixture() -> PathBuf {
    let path = std::env::temp_dir().join("senpai-cli-test.pdf");
    std::fs::write(&path, blank_pdf()).expect("write fixture");
    path
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_senpai"))
        .args(args)
        .output()
        .expect("run senpai");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn info_reports_page_count() {
    let pdf = fixture();
    let (ok, stdout) = run(&["info", pdf.to_str().unwrap()]);
    assert!(ok, "info failed: {stdout}");
    assert!(stdout.contains("pages:       1"), "{stdout}");
    assert!(stdout.contains("200.0 x 100.0 pt"), "{stdout}");
}

/// The bare `senpai <pdf>` form predates subcommands and must keep working.
#[test]
fn bare_path_still_means_info() {
    let pdf = fixture();
    let (ok, stdout) = run(&[pdf.to_str().unwrap()]);
    assert!(ok, "bare path failed: {stdout}");
    assert!(stdout.contains("pages:       1"), "{stdout}");
}

#[test]
fn bench_json_keeps_its_field_names() {
    let pdf = fixture();
    let (ok, stdout) = run(&[
        "bench",
        pdf.to_str().unwrap(),
        "--pages",
        "1",
        "--iters",
        "2",
        "--json",
    ]);
    assert!(ok, "bench failed: {stdout}");
    for field in [
        "\"file_bytes\"",
        "\"page_count\"",
        "\"mmap_us\"",
        "\"open_us\"",
        "\"time_to_first_page_us\"",
        "\"render\"",
        "\"page_text\"",
        "\"largest_page_bytes\"",
        "\"failures\": 0",
    ] {
        assert!(stdout.contains(field), "missing {field} in:\n{stdout}");
    }
    // --iters 2 over one sampled page.
    assert!(stdout.contains("\"n\":2"), "{stdout}");
}

#[test]
fn bench_rejects_a_missing_path() {
    let (ok, _) = run(&["bench", "--pages", "1"]);
    assert!(!ok, "bench with no path should fail");
}

#[test]
fn render_writes_a_ppm() {
    let pdf = fixture();
    let out = std::env::temp_dir().join("senpai-cli-test.ppm");
    let _ = std::fs::remove_file(&out);
    let (ok, stdout) = run(&[
        "render",
        pdf.to_str().unwrap(),
        out.to_str().unwrap(),
        "2.0",
    ]);
    assert!(ok, "render failed: {stdout}");
    let bytes = std::fs::read(&out).expect("ppm written");
    // P6 header for a 200x100pt page at 144 DPI, then 3 bytes per pixel.
    assert!(bytes.starts_with(b"P6\n400 200\n255\n"), "bad ppm header");
    assert_eq!(bytes.len(), b"P6\n400 200\n255\n".len() + 400 * 200 * 3);
}
