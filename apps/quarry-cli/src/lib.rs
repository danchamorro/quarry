use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use quarry_core::{IndexConfig, IndexProgress, OpenOptions, Session, StructuralIndex};

type CliResult<T> = Result<T, Box<dyn Error>>;

pub fn run(args: impl IntoIterator<Item = String>) -> CliResult<()> {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        Some("open") => open_command(args.collect()),
        Some("generate") => generate_command(args.collect()),
        Some("help" | "--help" | "-h") | None => {
            print_help();
            Ok(())
        }
        Some(command) => Err(format!("unknown command {command:?}; run quarry help").into()),
    }
}

fn print_help() {
    println!(
        "Quarry\n\n\
         Usage:\n  \
           quarry open <FILE> [--rows 100] [--delimiter ,] [--jump ROW] \
         [--jump-count 5] [--cache-state unknown|cold|warm] [--no-wait]\n  \
           quarry generate --size 10GB --columns 40 --delimiter , \
         --output FILE [--seed 1]"
    );
}

fn open_command(args: Vec<String>) -> CliResult<()> {
    let mut path = None;
    let mut rows = 100_usize;
    let mut delimiter = None;
    let mut jump: Option<u64> = None;
    let mut jump_count = 5_usize;
    let mut cache_state = "unknown".to_owned();
    let mut wait_for_index = true;
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--rows" => rows = value(&args, &mut cursor, "--rows")?.parse()?,
            "--delimiter" => {
                delimiter = Some(parse_delimiter(value(&args, &mut cursor, "--delimiter")?)?)
            }
            "--jump" => jump = Some(value(&args, &mut cursor, "--jump")?.parse()?),
            "--jump-count" => jump_count = value(&args, &mut cursor, "--jump-count")?.parse()?,
            "--cache-state" => {
                cache_state = value(&args, &mut cursor, "--cache-state")?.to_owned();
                if !matches!(cache_state.as_str(), "unknown" | "cold" | "warm") {
                    return Err("--cache-state must be unknown, cold, or warm".into());
                }
            }
            "--no-wait" => wait_for_index = false,
            option if option.starts_with('-') => {
                return Err(format!("unknown option {option:?}").into());
            }
            value if path.is_none() => path = Some(PathBuf::from(value)),
            value => return Err(format!("unexpected argument {value:?}").into()),
        }
        cursor += 1;
    }

    let path = path.ok_or("open requires a file path")?;
    let memory_before = current_rss_bytes();
    let session = Session::open(
        &path,
        OpenOptions {
            rows,
            delimiter,
            ..OpenOptions::default()
        },
    )?;
    let job = session.start_indexing(IndexConfig::default())?;
    let memory_at_first_rows = current_rss_bytes();

    println!("Quarry\n");
    println!("File: {}", session.path().display());
    println!("File size: {}", human_bytes(session.file_size));
    println!(
        "Build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("Cache state: {cache_state}");
    println!(
        "Detected delimiter: {}",
        display_delimiter(session.dialect.delimiter)
    );
    println!(
        "Detected header: {}",
        if session.dialect.has_header {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "Time to open: {:.3} ms",
        session.metrics.file_open.as_secs_f64() * 1000.0
    );
    println!(
        "Time to first {} rows: {:.3} ms",
        session.first_rows.len(),
        session.metrics.first_rows.as_secs_f64() * 1000.0
    );
    println!(
        "Bootstrap bytes read: {}",
        human_bytes(session.metrics.bootstrap_bytes_read)
    );
    println!("Current memory: {}", optional_bytes(memory_at_first_rows));
    println!("Memory before open: {}", optional_bytes(memory_before));
    println!();
    for (number, row) in session.first_rows.iter().take(5).enumerate() {
        let rendered = row
            .fields
            .iter()
            .map(|field| render_field(field))
            .collect::<Vec<_>>()
            .join(" | ");
        println!("row {number} @{}: {rendered}", row.offset);
    }

    if !wait_for_index {
        let cancel_started = Instant::now();
        job.cancel();
        let index = job.wait()?;
        println!(
            "\nIndexing cancelled after {} in {:.3} ms",
            human_bytes(index.indexed_bytes()),
            cancel_started.elapsed().as_secs_f64() * 1000.0
        );
        return Ok(());
    }

    println!("\nBackground indexing...");
    let mut last_report = Instant::now();
    let mut pending_jump = jump;
    loop {
        let progress = job.progress();
        if let Some(start) = pending_jump {
            let requested_end = start.saturating_add(jump_count as u64);
            if !progress.done && progress.rows_scanned >= requested_end {
                let index = job.snapshot();
                print_jump(&session, &index, start, jump_count, Some(progress))?;
                pending_jump = None;
            }
        }
        if progress.done {
            break;
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            println!(
                "  {:>6.2}%  {}  {} rows  {}/s",
                progress.bytes_scanned as f64 * 100.0 / progress.file_size.max(1) as f64,
                human_bytes(progress.bytes_scanned),
                progress.rows_scanned,
                human_bytes(rate(progress.bytes_scanned, progress.elapsed))
            );
            last_report = Instant::now();
        }
        thread::sleep(Duration::from_millis(100));
    }
    if let Some(error) = job.error() {
        return Err(error.into());
    }
    let progress = job.progress();
    let index = job.wait()?;

    println!("Bytes scanned: {}", human_bytes(progress.bytes_scanned));
    println!("Rows parsed: {}", index.indexed_rows());
    println!("Indexing time: {:.3} s", progress.elapsed.as_secs_f64());
    println!(
        "Indexing throughput: {}/s",
        human_bytes(rate(progress.bytes_scanned, progress.elapsed))
    );
    println!(
        "Index: {} checkpoints, every {} rows, {} memory",
        index.checkpoints().len(),
        index.checkpoint_every(),
        human_bytes(index.memory_bytes() as u64)
    );

    if let Some(start) = pending_jump {
        print_jump(&session, &index, start, jump_count, None)?;
    }

    println!("Current memory: {}", optional_bytes(current_rss_bytes()));
    println!("Peak memory: {}", optional_bytes(peak_rss_bytes()));
    Ok(())
}

fn print_jump(
    session: &Session,
    index: &StructuralIndex,
    start: u64,
    count: usize,
    live_progress: Option<IndexProgress>,
) -> CliResult<()> {
    let began = Instant::now();
    let selected = session.read_rows(index, start, count)?;
    let read_ms = began.elapsed().as_secs_f64() * 1000.0;
    if let Some(progress) = live_progress {
        println!(
            "Live jump to row {start} at {:.2}% indexed ({:.3} s): {} rows in {read_ms:.3} ms",
            progress.bytes_scanned as f64 * 100.0 / progress.file_size.max(1) as f64,
            progress.elapsed.as_secs_f64(),
            selected.len()
        );
    } else {
        println!(
            "Jump to row {start} after indexing: {} rows in {read_ms:.3} ms",
            selected.len()
        );
    }
    for (offset, row) in selected.iter().enumerate() {
        println!(
            "row {} @{}: {}",
            start + offset as u64,
            row.offset,
            row.fields
                .iter()
                .map(|field| render_field(field))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    Ok(())
}

fn generate_command(args: Vec<String>) -> CliResult<()> {
    let mut size = None;
    let mut columns = 40_usize;
    let mut delimiter = b',';
    let mut output = None;
    let mut seed = 1_u64;
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--size" => size = Some(parse_size(value(&args, &mut cursor, "--size")?)?),
            "--columns" => columns = value(&args, &mut cursor, "--columns")?.parse()?,
            "--delimiter" => {
                delimiter = parse_delimiter(value(&args, &mut cursor, "--delimiter")?)?
            }
            "--output" => output = Some(PathBuf::from(value(&args, &mut cursor, "--output")?)),
            "--seed" => seed = value(&args, &mut cursor, "--seed")?.parse()?,
            option => return Err(format!("unknown option {option:?}").into()),
        }
        cursor += 1;
    }

    let requested = size.ok_or("generate requires --size")?;
    let output = output.ok_or("generate requires --output")?;
    let began = Instant::now();
    let actual = generate_file(&output, requested, columns, delimiter, seed)?;
    println!(
        "Generated {} at {} in {:.3}s (seed {seed}, {columns} columns)",
        human_bytes(actual),
        output.display(),
        began.elapsed().as_secs_f64()
    );
    Ok(())
}

fn generate_file(
    output: &Path,
    requested: u64,
    columns: usize,
    delimiter: u8,
    seed: u64,
) -> CliResult<u64> {
    if requested == 0 || columns == 0 {
        return Err("size and columns must be non-zero".into());
    }
    parse_delimiter(std::str::from_utf8(&[delimiter])?)?;

    let file = File::create(output)?;
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);
    let mut row = Vec::with_capacity(columns.saturating_mul(24));
    for column in 0..columns {
        if column > 0 {
            row.push(delimiter);
        }
        write!(&mut row, "column_{}", column + 1)?;
    }
    row.push(b'\n');
    writer.write_all(&row)?;
    let mut written = row.len() as u64;
    let mut random = XorShift64::new(seed);
    let mut row_number = 0_u64;

    while written < requested {
        row.clear();
        for column in 0..columns {
            if column > 0 {
                row.push(delimiter);
            }
            let value = random.next();
            if row_number.is_multiple_of(211) && column == 1 {
                row.extend_from_slice(b"\"line one\nline two\"");
            } else if row_number.is_multiple_of(97) && column == 0 {
                row.push(b'"');
                write!(
                    &mut row,
                    "value{}{}with \"\"quotes\"\"",
                    delimiter as char, value
                )?;
                row.push(b'"');
            } else if row_number.is_multiple_of(503) && column == columns.saturating_sub(1) {
                row.extend(std::iter::repeat_n(b'x', 4096));
            } else {
                write!(&mut row, "{value:016x}")?;
            }
        }
        row.push(b'\n');
        writer.write_all(&row)?;
        written += row.len() as u64;
        row_number += 1;
    }
    writer.flush()?;
    Ok(written)
}

struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }
}

fn value<'a>(args: &'a [String], cursor: &mut usize, name: &str) -> CliResult<&'a str> {
    *cursor += 1;
    args.get(*cursor)
        .map(String::as_str)
        .ok_or_else(|| format!("{name} requires a value").into())
}

fn parse_delimiter(value: &str) -> CliResult<u8> {
    if matches!(value, "\\t" | "tab") {
        return Ok(b'\t');
    }
    let bytes = value.as_bytes();
    if bytes.len() != 1 || matches!(bytes[0], b'"' | b'\r' | b'\n') {
        return Err("delimiter must be one ASCII byte other than quote or newline".into());
    }
    Ok(bytes[0])
}

fn parse_size(value: &str) -> CliResult<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let number: f64 = value[..split].parse()?;
    let unit = value[split..].trim().to_ascii_uppercase();
    let multiplier = match unit.as_str() {
        "" | "B" => 1_f64,
        "KB" => 1_000_f64,
        "MB" => 1_000_000_f64,
        "GB" => 1_000_000_000_f64,
        "TB" => 1_000_000_000_000_f64,
        "KIB" => 1024_f64,
        "MIB" => 1024_f64.powi(2),
        "GIB" => 1024_f64.powi(3),
        "TIB" => 1024_f64.powi(4),
        _ => return Err(format!("unsupported size unit {unit:?}").into()),
    };
    let bytes = number * multiplier;
    if !bytes.is_finite() || bytes < 1.0 || bytes > u64::MAX as f64 {
        return Err("size must be between 1 byte and u64::MAX".into());
    }
    Ok(bytes as u64)
}

fn render_field(field: &[u8]) -> String {
    let mut rendered = String::from_utf8_lossy(field)
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    if rendered.chars().count() > 80 {
        rendered = rendered.chars().take(77).collect::<String>() + "...";
    }
    rendered
}

fn display_delimiter(delimiter: u8) -> String {
    match delimiter {
        b'\t' => "\\t".to_owned(),
        byte => (byte as char).to_string(),
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

fn rate(bytes: u64, duration: Duration) -> u64 {
    if duration.is_zero() {
        return 0;
    }
    (bytes as f64 / duration.as_secs_f64()) as u64
}

fn optional_bytes(bytes: Option<u64>) -> String {
    bytes
        .map(human_bytes)
        .unwrap_or_else(|| "unavailable".into())
}

fn current_rss_bytes() -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let kib: u64 = std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(kib * 1024)
}

fn peak_rss_bytes() -> Option<u64> {
    // SAFETY: `usage` is valid writable storage and getrusage initializes it for RUSAGE_SELF.
    let usage = unsafe {
        let mut usage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        usage
    };
    #[cfg(target_os = "macos")]
    return Some(usage.ru_maxrss as u64);
    #[cfg(not(target_os = "macos"))]
    return Some(usage.ru_maxrss as u64 * 1024);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{generate_file, parse_size};

    #[test]
    fn parses_sizes_and_generates_deterministically() {
        assert_eq!(parse_size("10GB").unwrap(), 10_000_000_000);
        assert_eq!(parse_size("1GiB").unwrap(), 1_073_741_824);

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("quarry-generator-{suffix}-1.csv"));
        let second = std::env::temp_dir().join(format!("quarry-generator-{suffix}-2.csv"));
        let first_size = generate_file(&first, 64 * 1024, 8, b',', 42).unwrap();
        let second_size = generate_file(&second, 64 * 1024, 8, b',', 42).unwrap();
        assert!(first_size >= 64 * 1024);
        assert_eq!(first_size, second_size);
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();
    }
}
