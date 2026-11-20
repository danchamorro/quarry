//! Subprocess checks for gzip-compressed inputs: read-only commands work through a
//! private copy, mutating commands refuse, and no temporary file outlives a command.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;

static NEXT: AtomicU64 = AtomicU64::new(0);

const CSV: &[u8] = b"name,value\r\nalpha,1\r\nbeta,2\r\ngamma,3\r\ndelta,4\r\n";

/// A process-unique fixture folder with a separate scratch folder for private copies.
struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "quarry-cli-compressed-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        Self(root)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn scratch(&self) -> PathBuf {
        self.0.join("scratch")
    }

    /// Every private decompressed copy lives in a `.quarry-compressed-*` folder.
    fn leftovers(&self) -> Vec<String> {
        std::fs::read_dir(self.scratch())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// Runs quarry with `--temp-dir` pointing at the scratch folder.
    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_quarry"))
            .args(args)
            .arg("--temp-dir")
            .arg(self.scratch())
            .output()
            .unwrap();
        assert!(
            self.leftovers().is_empty(),
            "quarry {args:?} left temporary files: {:?}",
            self.leftovers()
        );
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn failure(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "expected failure, got:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A two-member gzip file, so every command also covers concatenated members.
fn write_source(fixture: &Fixture) -> PathBuf {
    let source = fixture.path("sales.csv.gz");
    let split = CSV.len() / 2;
    let mut bytes = gzip(&CSV[..split]);
    bytes.extend(gzip(&CSV[split..]));
    std::fs::write(&source, bytes).unwrap();
    source
}

#[test]
fn read_only_commands_open_gzip_through_a_private_copy() {
    let fixture = Fixture::new();
    let source = write_source(&fixture);
    let source_bytes = std::fs::read(&source).unwrap();
    let gz = text(&source);

    let open = success(&fixture.run(&["open", &gz, "--jump", "2", "--jump-count", "2"]));
    assert!(open.contains("Compression: Gzip"), "{open}");
    assert!(open.contains(&format!(
        "Compressed size: {} B ({} bytes)",
        format_args!("{:.2}", source_bytes.len() as f64),
        source_bytes.len()
    )));
    assert!(open.contains(&format!(
        "Decompressed size: {:.2} B ({} bytes)",
        CSV.len() as f64,
        CSV.len()
    )));
    assert!(open.contains("Rows parsed: 5"), "{open}");
    assert!(open.contains("Columns: 2"), "{open}");
    assert!(
        open.contains("row 2 @") && open.contains("beta | 2"),
        "{open}"
    );
    assert!(open.contains("Decompression and indexing time:"), "{open}");

    let no_wait = success(&fixture.run(&["open", &gz, "--no-wait"]));
    assert!(no_wait.contains("Compression: Gzip"), "{no_wait}");
    assert!(
        no_wait.contains("Decompression skipped (--no-wait)"),
        "{no_wait}"
    );
    assert!(
        no_wait.contains("row 1 @") && no_wait.contains("alpha | 1"),
        "{no_wait}"
    );
    assert!(!no_wait.contains("Rows parsed"), "{no_wait}");

    let viewport = success(&fixture.run(&["viewport", &gz, "--iterations", "3", "--rows", "2"]));
    assert!(viewport.contains(&format!("File: {gz}")), "{viewport}");
    assert!(viewport.contains("Compression: Gzip") && viewport.contains("Rows indexed: 5"));

    let search = success(&fixture.run(&["search", &gz, "--query", "gamma"]));
    assert!(search.contains("Compression: Gzip"), "{search}");
    assert!(search.contains("Match: data row 3, column 1"), "{search}");

    let filter = success(&fixture.run(&[
        "filter",
        &gz,
        "--column",
        "2",
        "--operator",
        "gte",
        "--value",
        "3",
    ]));
    assert!(filter.contains("Matches found: 2"), "{filter}");

    let exported = fixture.path("filtered.csv");
    let export = success(&fixture.run(&[
        "export",
        &gz,
        "--output",
        &text(&exported),
        "--column",
        "1",
        "--operator",
        "equals",
        "--value",
        "beta",
    ]));
    assert!(export.contains(&format!("Source: {gz}")), "{export}");
    assert_eq!(
        std::fs::read(&exported).unwrap(),
        b"name,value\r\nbeta,2\r\n"
    );

    let converted = fixture.path("sales.csv");
    let convert = success(&fixture.run(&["convert-utf8", &gz, "--output", &text(&converted)]));
    assert!(convert.contains("Outcome: complete"), "{convert}");
    assert_eq!(std::fs::read(&converted).unwrap(), CSV);

    let inspect = success(&fixture.run(&["inspect", &gz]));
    assert!(inspect.contains(&format!("File: {gz}")), "{inspect}");
    assert!(inspect.contains("Inspection: Complete") && inspect.contains("Records: 5"));
    assert!(inspect.contains("Compression: Gzip"), "{inspect}");

    assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
}

#[test]
fn decompression_limit_and_damage_fail_without_leftovers() {
    let fixture = Fixture::new();
    let source = write_source(&fixture);
    let gz = text(&source);
    let exported = fixture.path("out.csv");

    let limited = failure(&fixture.run(&["open", &gz, "--max-decompressed-bytes", "20"]));
    assert!(limited.contains("limit"), "{limited}");
    let limited = failure(&fixture.run(&[
        "search",
        &gz,
        "--query",
        "beta",
        "--max-decompressed-bytes",
        "40B",
    ]));
    assert!(limited.contains("limit"), "{limited}");
    success(&fixture.run(&["open", &gz, "--max-decompressed-bytes", "1KB"]));

    let valid = gzip(CSV);
    let mut truncated = valid.clone();
    truncated.truncate(valid.len() - 5);
    let mut bad_crc = valid.clone();
    let crc_at = valid.len() - 8;
    bad_crc[crc_at] ^= 0xff;
    let mut trailing = valid.clone();
    trailing.extend_from_slice(b"garbage");
    for (name, bytes) in [
        ("truncated.csv.gz", truncated),
        ("bad-crc.csv.gz", bad_crc),
        ("trailing.csv.gz", trailing),
        ("renamed.csv.gz", CSV.to_vec()),
    ] {
        let damaged = fixture.path(name);
        std::fs::write(&damaged, bytes).unwrap();
        let damaged = text(&damaged);
        for args in [
            vec!["open", damaged.as_str()],
            vec!["search", damaged.as_str(), "--query", "beta"],
            vec![
                "convert-utf8",
                damaged.as_str(),
                "--output",
                exported.to_str().unwrap(),
            ],
            vec!["inspect", damaged.as_str()],
        ] {
            let error = failure(&fixture.run(&args));
            assert!(error.starts_with("error:"), "{name} {args:?}: {error}");
            assert!(!exported.exists(), "{name} {args:?} published output");
        }
    }
}

#[test]
fn compressed_inputs_are_read_only_and_outputs_are_never_gzip() {
    let fixture = Fixture::new();
    let source = write_source(&fixture);
    let gz = text(&source);
    let plain = fixture.path("plain.csv");
    std::fs::write(&plain, CSV).unwrap();
    let plain = text(&plain);
    let output = fixture.path("out.csv");
    let out = text(&output);

    for args in [
        vec![
            "edit-save-as",
            gz.as_str(),
            "--output",
            out.as_str(),
            "--edit",
            "1",
            "1",
            "x",
        ],
        vec![
            "replace-all-save-as",
            gz.as_str(),
            "--output",
            out.as_str(),
            "--query",
            "a",
            "--replacement",
            "b",
        ],
        vec![
            "transform-save-as",
            gz.as_str(),
            "--output",
            out.as_str(),
            "--join",
            "1,2",
            "-",
        ],
        vec![
            "sort-save-as",
            gz.as_str(),
            out.as_str(),
            "--column",
            "1",
            "--order",
            "asc",
        ],
        vec!["duplicates", gz.as_str(), "--columns", "1"],
        vec![
            "convert-utf8",
            gz.as_str(),
            "--output",
            out.as_str(),
            "--edit",
            "1",
            "1",
            "x",
        ],
    ] {
        // Mutating commands do not take --temp-dir, so run them without it.
        let result = Command::new(env!("CARGO_BIN_EXE_quarry"))
            .args(&args)
            .output()
            .unwrap();
        let error = failure(&result);
        assert!(error.contains("read-only"), "{args:?}: {error}");
        assert!(!output.exists(), "{args:?} published output");
    }

    let gz_output = fixture.path("out.csv.gz");
    let gz_out = text(&gz_output);
    for (args, temp) in [
        (
            vec!["convert-utf8", gz.as_str(), "--output", gz_out.as_str()],
            true,
        ),
        (
            vec![
                "export",
                gz.as_str(),
                "--output",
                gz_out.as_str(),
                "--column",
                "1",
                "--operator",
                "equals",
                "--value",
                "beta",
            ],
            true,
        ),
        (
            vec!["convert-utf8", plain.as_str(), "--output", gz_out.as_str()],
            true,
        ),
        (
            vec![
                "sort-save-as",
                plain.as_str(),
                gz_out.as_str(),
                "--column",
                "1",
                "--order",
                "asc",
            ],
            false,
        ),
        (
            vec![
                "duplicates",
                plain.as_str(),
                "--columns",
                "1",
                "--output",
                gz_out.as_str(),
            ],
            false,
        ),
    ] {
        let result = if temp {
            fixture.run(&args)
        } else {
            Command::new(env!("CARGO_BIN_EXE_quarry"))
                .args(&args)
                .output()
                .unwrap()
        };
        let error = failure(&result);
        assert!(error.contains("without .gz"), "{args:?}: {error}");
        assert!(!gz_output.exists(), "{args:?} wrote a .gz output");
    }

    let live = failure(&fixture.run(&["viewport", &gz, "--live"]));
    assert!(live.contains("--live"), "{live}");
    let unsupported = fixture.path("data.csv.zst");
    std::fs::write(&unsupported, [0x28, 0xb5, 0x2f, 0xfd, 0, 0, 0, 0]).unwrap();
    let error = failure(&fixture.run(&["open", &text(&unsupported)]));
    assert!(error.contains("gzip"), "{error}");
}

/// Ctrl-C during decompression stops the job and removes the private copy.
#[cfg(unix)]
#[test]
fn interrupting_decompression_removes_the_private_copy() {
    let fixture = Fixture::new();
    let source = fixture.path("large.csv.gz");
    // Repeat one small compressed member: tiny on disk, 256 MiB once expanded, so the
    // job is still running when the signal arrives.
    let mut block = b"id,name,amount\n".to_vec();
    while block.len() < (1 << 20) - 64 {
        block.extend_from_slice(b"1,customer,7.00\n");
    }
    let member = gzip(&block);
    let mut file = std::fs::File::create(&source).unwrap();
    for _ in 0..256 {
        file.write_all(&member).unwrap();
    }
    drop(file);

    let mut child = Command::new(env!("CARGO_BIN_EXE_quarry"))
        .arg("open")
        .arg(&source)
        .arg("--temp-dir")
        .arg(fixture.scratch())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while fixture.leftovers().is_empty() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "quarry finished before it could be interrupted"
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "no private copy appeared"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: the pid belongs to the child this test spawned and still owns.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    let output = child.wait_with_output().unwrap();
    let error = failure(&output);
    assert!(error.contains("interrupted"), "{error}");
    assert!(fixture.leftovers().is_empty(), "{:?}", fixture.leftovers());
}
