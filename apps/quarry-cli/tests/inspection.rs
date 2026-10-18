use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct TestDirectory(PathBuf);
impl TestDirectory {
    /// Creates a process-unique synthetic fixture directory.
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "quarry-cli-inspect-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
    /// Returns the fixture root for subprocess tests.
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TestDirectory {
    /// Removes only this test-owned directory.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
use std::process::Command;
#[test]
/// Verifies that inspect reports and recovers without replacing source.
fn inspect_reports_and_recovers_without_replacing_source() {
    let dir = TestDirectory::new();
    let source = dir.path().join("source.csv");
    let output = dir.path().join("recovered.csv");
    let bytes = b"name,id\nAlice,1\nbad\nBob,2\n";
    std::fs::write(&source, bytes).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_quarry"))
        .args(["inspect"])
        .arg(&source)
        .args([
            "--delimiter",
            ",",
            "--header",
            "first-row",
            "--details",
            "--omit-findings",
            "1",
            "--output",
        ])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(
        text.contains("Inspection: Complete")
            && text.contains("Record: 3")
            && text.contains("Removed records: 1")
    );
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    assert_eq!(std::fs::read(output).unwrap(), b"name,id\nAlice,1\nBob,2\n");
}
#[test]
/// Verifies that malformed inspection is partial and never claims a clean file.
fn malformed_inspection_is_partial_and_never_claims_a_clean_file() {
    let dir = TestDirectory::new();
    let source = dir.path().join("bad.csv");
    std::fs::write(&source, b"a,b\n1,\"bad\n2,3\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_quarry"))
        .arg("inspect")
        .arg(source)
        .args(["--delimiter", ",", "--header", "first-row"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.contains("Inspection: Stopped") && text.contains("without a closing quote"));
}

#[test]
/// Verifies that late sort error reports data location and preserves source.
fn late_sort_error_reports_data_location_and_preserves_source() {
    let dir = TestDirectory::new();
    let source = dir.path().join("source.csv");
    let output = dir.path().join("sorted.csv");
    let mut bytes = b"name,number\n".to_vec();
    for i in 1..=30 {
        bytes.extend_from_slice(format!("row{i},{i}\n").as_bytes());
    }
    bytes.extend_from_slice(b"bad,not-a-number\n");
    std::fs::write(&source, &bytes).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_quarry"))
        .arg("sort-save-as")
        .arg(&source)
        .arg(&output)
        .args([
            "--header",
            "first-row",
            "--column",
            "2",
            "--order",
            "asc",
            "--mode",
            "number",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let details = String::from_utf8(result.stderr).unwrap();
    assert!(details.contains("Data row: 31"), "{details}");
    assert!(details.contains("Column: 2"), "{details}");
    assert!(
        details.contains("Original-file byte position: unavailable"),
        "{details}"
    );
    assert!(!output.exists());
    assert_eq!(std::fs::read(source).unwrap(), bytes);
}

#[test]
/// Verifies that help has separate command lines.
fn help_has_separate_command_lines() {
    let out = Command::new(env!("CARGO_BIN_EXE_quarry"))
        .arg("--help")
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.lines()
            .any(|l| l.trim_start().starts_with("quarry open <FILE>"))
    );
    assert!(!text.contains("\\n"));
}
