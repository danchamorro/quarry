use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use quarry_core::{
    FilterIndex, FilterJob, FilterMatch, FilterOperator, FilterProgress, FilterQuery, IndexConfig,
    IndexJob, IndexProgress, OpenOptions, SearchJob, SearchOutcome, SearchPosition, SearchProgress,
    Session, StructuralIndex,
};

type CliResult<T> = Result<T, Box<dyn Error>>;
type SearchRun = (SearchOutcome, SearchProgress, Option<(u64, Duration)>);
type FilterRun = (FilterIndex, FilterProgress, Option<(u64, Duration)>);

const MAX_LIVE_BENCHMARK_MILLIS: u128 = 60_000;
const FILTER_SAMPLE_ROWS: usize = 100;

pub fn run(args: impl IntoIterator<Item = String>) -> CliResult<()> {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        Some("open") => open_command(args.collect()),
        Some("viewport") => viewport_command(args.collect()),
        Some("search") => search_command(args.collect()),
        Some("filter") => filter_command(args.collect()),
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
           quarry viewport <FILE> [--iterations 500] [--rows 100] \
         [--seed 1] [--cache-state unknown|cold|warm] [--live] \
         [--interval-ms 16] [--chunk-bytes 1048576]\n  \
           quarry search <FILE> --query LITERAL [--start-row 1] \
         [--start-column 1] [--cancel-after-bytes N] \
         [--cache-state unknown|warm]\n  \
           quarry filter <FILE> --column N --operator contains|equals \
         --value LITERAL [--cancel-after-bytes N] \
         [--cache-state unknown|cold|warm]\n  \
           quarry generate --size 10GB --columns 40 --delimiter , \
         --output FILE [--seed 1]"
    );
}

fn filter_command(args: Vec<String>) -> CliResult<()> {
    let mut path = None;
    let mut column = None;
    let mut operator = None;
    let mut filter_value = None;
    let mut cancel_after_bytes = None;
    let mut cache_state = "unknown".to_owned();
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--column" => column = Some(value(&args, &mut cursor, "--column")?.parse::<usize>()?),
            "--operator" => {
                operator = Some(match value(&args, &mut cursor, "--operator")? {
                    "contains" => FilterOperator::Contains,
                    "equals" => FilterOperator::Equals,
                    _ => return Err("--operator must be contains or equals".into()),
                })
            }
            "--value" => {
                filter_value = Some(value(&args, &mut cursor, "--value")?.as_bytes().to_vec())
            }
            "--cancel-after-bytes" => {
                cancel_after_bytes =
                    Some(value(&args, &mut cursor, "--cancel-after-bytes")?.parse::<u64>()?)
            }
            "--cache-state" => {
                cache_state = value(&args, &mut cursor, "--cache-state")?.to_owned();
                if !matches!(cache_state.as_str(), "unknown" | "cold" | "warm") {
                    return Err("--cache-state must be unknown, cold, or warm".into());
                }
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown option {option:?}").into());
            }
            value if path.is_none() => path = Some(PathBuf::from(value)),
            value => return Err(format!("unexpected argument {value:?}").into()),
        }
        cursor += 1;
    }

    let path = path.ok_or("filter requires a file path")?;
    let column = column.ok_or("filter requires --column")?;
    let operator = operator.ok_or("filter requires --operator")?;
    let filter_value = filter_value.ok_or("filter requires --value")?;
    if column == 0 {
        return Err("filter column must be at least 1".into());
    }
    if operator == FilterOperator::Contains && filter_value.is_empty() {
        return Err("contains filter value must not be empty".into());
    }
    if cancel_after_bytes == Some(0) {
        return Err("cancel-after-bytes must be non-zero".into());
    }

    let session = Session::open(&path, OpenOptions::default())?;
    if cancel_after_bytes.is_some_and(|bytes| bytes >= session.file_size) {
        return Err("cancel-after-bytes must be less than file size".into());
    }
    let query = FilterQuery {
        column: column - 1,
        operator,
        value: filter_value,
    };
    let job = session.start_filter(query)?;
    let (index, progress, cancellation) =
        wait_for_filter(job, cancel_after_bytes, Duration::from_millis(1))?;
    let samples = sample_filtered_rows(&session, &index)?;

    println!("Quarry filter benchmark\n");
    println!("File: {}", session.path().display());
    println!(
        "File size: {} ({} bytes)",
        human_bytes(session.file_size),
        session.file_size
    );
    println!(
        "Build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("Filter cache state: {cache_state}");
    println!("Column: {column}");
    println!(
        "Operator: {}",
        match operator {
            FilterOperator::Contains => "contains",
            FilterOperator::Equals => "equals",
        }
    );
    println!("Value: {}", render_field(&index.query().value));
    println!(
        "Outcome: {}",
        if progress.cancelled {
            "cancelled"
        } else {
            "complete"
        }
    );
    println!("Matches found: {}", index.matches_found());
    println!(
        "Bytes scanned: {} ({} bytes) of {} ({} bytes)",
        human_bytes(progress.bytes_scanned),
        progress.bytes_scanned,
        human_bytes(progress.file_size),
        progress.file_size
    );
    println!("Physical records scanned: {}", progress.rows_scanned);
    println!("Filter time: {:.3} s", progress.elapsed.as_secs_f64());
    println!(
        "Filter throughput: {}/s",
        human_bytes(rate(progress.bytes_scanned, progress.elapsed))
    );
    println!(
        "Filter index memory: {}",
        human_bytes(index.memory_bytes() as u64)
    );
    match samples.first {
        Some(first) => println!(
            "First match: ordinal {}, data row {}, record offset {}",
            first.match_ordinal + 1,
            physical_to_data_row(first.row, session.dialect.has_header),
            first.record_offset
        ),
        None => println!("First match: none"),
    }
    match samples.last {
        Some(last) => println!(
            "Last sampled match: ordinal {}, data row {}, record offset {}",
            last.match_ordinal + 1,
            physical_to_data_row(last.row, session.dialect.has_header),
            last.record_offset
        ),
        None => println!("Last sampled match: none"),
    }
    println!("Bounded sample rows read: {}", samples.rows_read);
    println!(
        "Bounded sample read time: {:.3} ms",
        samples.elapsed.as_secs_f64() * 1000.0
    );
    println!("Sample checksum: {}", samples.checksum);
    if let Some((requested_at, latency)) = cancellation {
        println!(
            "Cancellation requested after: {} ({} bytes)",
            human_bytes(requested_at),
            requested_at
        );
        println!(
            "Poll-inclusive cancellation latency: {:.3} ms",
            latency.as_secs_f64() * 1000.0
        );
    }
    println!(
        "Current process memory: {}",
        optional_bytes(current_rss_bytes())
    );
    println!(
        "Peak process memory (filter): {}",
        optional_bytes(peak_rss_bytes())
    );
    Ok(())
}

fn wait_for_filter(
    job: FilterJob,
    cancel_after_bytes: Option<u64>,
    poll_interval: Duration,
) -> CliResult<FilterRun> {
    let mut cancellation = None;
    loop {
        let progress = job.progress();
        if progress.done {
            break;
        }
        if cancellation.is_none()
            && cancel_after_bytes.is_some_and(|threshold| progress.bytes_scanned >= threshold)
        {
            let started = Instant::now();
            job.cancel();
            cancellation = Some((progress.bytes_scanned, started));
        }
        thread::sleep(poll_interval);
    }

    let progress = job.progress();
    let index = job.wait()?;
    let cancellation = cancellation.map(|(bytes, started)| (bytes, started.elapsed()));
    if cancel_after_bytes.is_some() && cancellation.is_none() {
        return Err("filter finished before cancellation threshold".into());
    }
    if cancel_after_bytes.is_some() && !progress.cancelled {
        return Err("filter completed before cancellation took effect".into());
    }
    if cancel_after_bytes.is_some() && progress.bytes_scanned >= progress.file_size {
        return Err("filter reached end of file before cancellation took effect".into());
    }
    Ok((index, progress, cancellation))
}

#[derive(Clone, Copy)]
struct FilterLocation {
    match_ordinal: u64,
    row: u64,
    record_offset: u64,
}

#[derive(Default)]
struct FilterSamples {
    first: Option<FilterLocation>,
    last: Option<FilterLocation>,
    rows_read: usize,
    checksum: u64,
    elapsed: Duration,
}

fn sample_filtered_rows(session: &Session, index: &FilterIndex) -> CliResult<FilterSamples> {
    let matches = index.matches_found();
    if matches == 0 {
        return Ok(FilterSamples::default());
    }

    let mut starts = [
        0,
        matches / 2,
        matches.saturating_sub(FILTER_SAMPLE_ROWS as u64),
    ];
    starts.sort_unstable();
    let started = Instant::now();
    let mut samples = FilterSamples::default();
    for (position, start) in starts.into_iter().enumerate() {
        if position > 0 && start == starts[position - 1] {
            continue;
        }
        let count = usize::try_from((matches - start).min(FILTER_SAMPLE_ROWS as u64))?;
        for found in session.read_filtered_rows(index, start, count)? {
            record_filter_sample(&mut samples, &found);
        }
    }
    samples.elapsed = started.elapsed();
    Ok(samples)
}

fn record_filter_sample(samples: &mut FilterSamples, found: &FilterMatch) {
    let location = FilterLocation {
        match_ordinal: found.match_ordinal,
        row: found.row,
        record_offset: found.record_offset,
    };
    samples.first.get_or_insert(location);
    samples.last = Some(location);
    samples.rows_read += 1;
    samples.checksum = samples
        .checksum
        .rotate_left(7)
        .wrapping_add(found.match_ordinal)
        ^ found.row.rotate_left(17)
        ^ found.record_offset;
    for field in &found.fields {
        for &byte in field {
            samples.checksum = samples.checksum.wrapping_mul(1_099_511_628_211) ^ u64::from(byte);
        }
    }
}

fn search_command(args: Vec<String>) -> CliResult<()> {
    let mut path = None;
    let mut query = None;
    let mut start_row = 1_u64;
    let mut start_column = 1_usize;
    let mut cancel_after_bytes = None;
    let mut cache_state = "unknown".to_owned();
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--query" => query = Some(value(&args, &mut cursor, "--query")?.as_bytes().to_vec()),
            "--start-row" => start_row = value(&args, &mut cursor, "--start-row")?.parse()?,
            "--start-column" => {
                start_column = value(&args, &mut cursor, "--start-column")?.parse()?
            }
            "--cancel-after-bytes" => {
                cancel_after_bytes =
                    Some(value(&args, &mut cursor, "--cancel-after-bytes")?.parse::<u64>()?)
            }
            "--cache-state" => {
                cache_state = value(&args, &mut cursor, "--cache-state")?.to_owned();
                if !matches!(cache_state.as_str(), "unknown" | "cold" | "warm") {
                    return Err("--cache-state must be unknown, cold, or warm".into());
                }
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown option {option:?}").into());
            }
            value if path.is_none() => path = Some(PathBuf::from(value)),
            value => return Err(format!("unexpected argument {value:?}").into()),
        }
        cursor += 1;
    }

    let path = path.ok_or("search requires a file path")?;
    let query = query.ok_or("search requires --query")?;
    if query.is_empty() {
        return Err("query must not be empty".into());
    }
    if start_row == 0 || start_column == 0 {
        return Err("start row and column must be at least 1".into());
    }
    if cancel_after_bytes == Some(0) {
        return Err("cancel-after-bytes must be non-zero".into());
    }
    if cache_state == "cold" {
        return Err("search cannot be cold because indexing reads the file first".into());
    }

    let session = Session::open(&path, OpenOptions::default())?;
    let start = data_search_position(start_row, start_column, session.dialect.has_header)?;
    let index_started = Instant::now();
    let index = session.start_indexing(IndexConfig::default())?.wait()?;
    let index_elapsed = index_started.elapsed();
    let search_bytes = session
        .file_size
        .saturating_sub(index.nearest_checkpoint(start.row).offset);
    if cancel_after_bytes.is_some_and(|bytes| bytes >= search_bytes) {
        return Err("cancel-after-bytes must be less than the searchable byte span".into());
    }
    let job = session.start_search(&index, query.clone(), start)?;
    let (outcome, progress, cancellation) =
        wait_for_search(job, cancel_after_bytes, Duration::from_millis(1))?;

    println!("Quarry search benchmark\n");
    println!("File: {}", session.path().display());
    println!(
        "File size: {} ({} bytes)",
        human_bytes(session.file_size),
        session.file_size
    );
    println!(
        "Build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("Search cache state: {cache_state} after indexing prepass");
    println!("Query: {}", render_field(&query));
    println!("Start: data row {start_row}, column {start_column}");
    println!("Rows indexed: {}", index.indexed_rows());
    println!("Indexing time: {:.3} s", index_elapsed.as_secs_f64());
    let time_label = if matches!(&outcome, SearchOutcome::Match(_)) {
        "Time to first match"
    } else {
        "Search time"
    };
    match outcome {
        SearchOutcome::Match(found) => {
            let data_row = physical_to_data_row(found.row, session.dialect.has_header);
            println!("Outcome: match");
            println!(
                "Match: data row {data_row}, column {}, record offset {}",
                found.column + 1,
                found.record_offset
            );
        }
        SearchOutcome::NotFound => println!("Outcome: not found"),
        SearchOutcome::Cancelled => println!("Outcome: cancelled"),
    }
    println!(
        "Bytes scanned: {} ({} bytes) of {} ({} bytes)",
        human_bytes(progress.bytes_scanned),
        progress.bytes_scanned,
        human_bytes(progress.total_bytes),
        progress.total_bytes
    );
    println!("Rows scanned: {}", progress.rows_scanned);
    println!("{time_label}: {:.3} s", progress.elapsed.as_secs_f64());
    println!(
        "Search throughput: {}/s",
        human_bytes(rate(progress.bytes_scanned, progress.elapsed))
    );
    if let Some((requested_at, latency)) = cancellation {
        println!(
            "Cancellation requested after: {} ({} bytes)",
            human_bytes(requested_at),
            requested_at
        );
        println!(
            "Poll-inclusive cancellation latency: {:.3} ms",
            latency.as_secs_f64() * 1000.0
        );
    }
    println!(
        "Current process memory: {}",
        optional_bytes(current_rss_bytes())
    );
    println!(
        "Peak process memory (index + search): {}",
        optional_bytes(peak_rss_bytes())
    );
    Ok(())
}

fn data_search_position(
    data_row: u64,
    column: usize,
    has_header: bool,
) -> CliResult<SearchPosition> {
    let row = data_row
        .checked_sub(1)
        .and_then(|row| row.checked_add(u64::from(has_header)))
        .ok_or("search position is out of range")?;
    let column = column
        .checked_sub(1)
        .ok_or("search position is out of range")?;
    Ok(SearchPosition { row, column })
}

fn physical_to_data_row(row: u64, has_header: bool) -> u64 {
    if has_header {
        row
    } else {
        row.saturating_add(1)
    }
}

fn wait_for_search(
    job: SearchJob,
    cancel_after_bytes: Option<u64>,
    poll_interval: Duration,
) -> CliResult<SearchRun> {
    let mut cancellation = None;
    loop {
        let progress = job.progress();
        if progress.done {
            break;
        }
        if cancellation.is_none()
            && cancel_after_bytes.is_some_and(|threshold| progress.bytes_scanned >= threshold)
        {
            let started = Instant::now();
            job.cancel();
            cancellation = Some((progress.bytes_scanned, started));
        }
        thread::sleep(poll_interval);
    }

    let progress = job.progress();
    let outcome = job.wait()?;
    let cancellation = cancellation.map(|(bytes, started)| (bytes, started.elapsed()));
    if cancel_after_bytes.is_some() && cancellation.is_none() {
        return Err("search finished before cancellation threshold".into());
    }
    if cancel_after_bytes.is_some() && !matches!(outcome, SearchOutcome::Cancelled) {
        return Err("search completed before cancellation took effect".into());
    }
    Ok((outcome, progress, cancellation))
}

fn viewport_command(args: Vec<String>) -> CliResult<()> {
    let mut path = None;
    let mut iterations = 500_usize;
    let mut rows = 100_usize;
    let mut seed = 1_u64;
    let mut cache_state = "unknown".to_owned();
    let mut live = false;
    let mut interval_ms = 16_u64;
    let mut chunk_bytes = IndexConfig::default().chunk_bytes;
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--iterations" => iterations = value(&args, &mut cursor, "--iterations")?.parse()?,
            "--rows" => rows = value(&args, &mut cursor, "--rows")?.parse()?,
            "--seed" => seed = value(&args, &mut cursor, "--seed")?.parse()?,
            "--live" => live = true,
            "--interval-ms" => interval_ms = value(&args, &mut cursor, "--interval-ms")?.parse()?,
            "--chunk-bytes" => chunk_bytes = value(&args, &mut cursor, "--chunk-bytes")?.parse()?,
            "--cache-state" => {
                cache_state = value(&args, &mut cursor, "--cache-state")?.to_owned();
                if !matches!(cache_state.as_str(), "unknown" | "cold" | "warm") {
                    return Err("--cache-state must be unknown, cold, or warm".into());
                }
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown option {option:?}").into());
            }
            value if path.is_none() => path = Some(PathBuf::from(value)),
            value => return Err(format!("unexpected argument {value:?}").into()),
        }
        cursor += 1;
    }

    if iterations == 0 || rows == 0 {
        return Err("iterations and rows must be non-zero".into());
    }
    if interval_ms == 0 || chunk_bytes == 0 {
        return Err("interval and chunk bytes must be non-zero".into());
    }
    if live && interval_ms as u128 * iterations as u128 > MAX_LIVE_BENCHMARK_MILLIS {
        return Err("live viewport schedule must not exceed 60 seconds".into());
    }
    let path = path.ok_or("viewport requires a file path")?;
    let session = Session::open(
        &path,
        OpenOptions {
            rows,
            ..OpenOptions::default()
        },
    )?;
    let index_started = Instant::now();
    let job = session.start_indexing(IndexConfig {
        chunk_bytes,
        ..IndexConfig::default()
    })?;
    if live {
        return benchmark_live_viewports(
            &session,
            job,
            iterations,
            rows,
            Duration::from_millis(interval_ms),
            chunk_bytes,
            &cache_state,
        );
    }
    let index = job.wait()?;
    let index_elapsed = index_started.elapsed();
    if index.indexed_rows() < rows as u64 {
        return Err(format!(
            "viewport requires {rows} rows, but the file contains {}",
            index.indexed_rows()
        )
        .into());
    }

    let max_start = index.indexed_rows() - rows as u64;
    let repeated_start = max_start / 2;
    let (repeated, repeated_checksum) =
        benchmark_viewports(&session, &index, iterations, rows, |_| repeated_start)?;

    let mut sequential_start = repeated_start;
    let (sequential, sequential_checksum) =
        benchmark_viewports(&session, &index, iterations, rows, |_| {
            let start = sequential_start;
            sequential_start = sequential_start
                .checked_add(rows as u64)
                .filter(|next| *next <= max_start)
                .unwrap_or(0);
            start
        })?;

    let mut random = XorShift64::new(seed);
    let (random_stats, random_checksum) =
        benchmark_viewports(&session, &index, iterations, rows, |_| {
            random.next() % max_start.saturating_add(1)
        })?;

    println!("Quarry viewport benchmark\n");
    println!("File: {}", session.path().display());
    println!("File size: {}", human_bytes(session.file_size));
    println!("Rows indexed: {}", index.indexed_rows());
    println!("Indexing time: {:.3} s", index_elapsed.as_secs_f64());
    println!("Cache state: {cache_state}");
    println!("Iterations: {iterations} per pattern, {rows} rows per viewport");
    println!("Seed: {seed}\n");
    println!("Pattern       min ms    p50 ms    p95 ms    max ms   requests/s");
    print_viewport_stats("repeated", repeated);
    print_viewport_stats("sequential", sequential);
    print_viewport_stats("random", random_stats);
    println!(
        "Checksum: {}",
        repeated_checksum ^ sequential_checksum ^ random_checksum
    );
    println!("Current memory: {}", optional_bytes(current_rss_bytes()));
    println!("Peak memory: {}", optional_bytes(peak_rss_bytes()));
    Ok(())
}

#[derive(Clone, Copy)]
struct LatencyStats {
    min: Duration,
    p50: Duration,
    p95: Duration,
    max: Duration,
    total: Duration,
    count: usize,
}

fn benchmark_viewports(
    session: &Session,
    index: &StructuralIndex,
    iterations: usize,
    rows: usize,
    mut start: impl FnMut(usize) -> u64,
) -> CliResult<(LatencyStats, u64)> {
    let mut samples = Vec::with_capacity(iterations);
    let mut checksum = 0_u64;
    for iteration in 0..iterations {
        let start = start(iteration);
        let began = Instant::now();
        let selected = session.read_rows(index, start, rows)?;
        samples.push(began.elapsed());
        checksum = selected.iter().fold(checksum, |checksum, row| {
            row.fields
                .iter()
                .fold(checksum ^ row.offset, |sum, field| sum ^ field.len() as u64)
        });
    }
    Ok((latency_stats(&mut samples), checksum))
}

fn benchmark_live_viewports(
    session: &Session,
    job: IndexJob,
    iterations: usize,
    rows: usize,
    interval: Duration,
    chunk_bytes: usize,
    cache_state: &str,
) -> CliResult<()> {
    let required_rows = (iterations as u64)
        .checked_mul(rows as u64)
        .ok_or("live viewport workload is too large")?;
    let ready = loop {
        let progress = job.progress();
        if progress.done {
            return live_finished_early(
                job,
                "indexing finished before the live viewport workload was ready",
            );
        }
        if progress.rows_scanned >= required_rows {
            break progress;
        }
        thread::sleep(Duration::from_millis(1));
    };

    let mut snapshot_samples = Vec::with_capacity(iterations);
    let mut read_samples = Vec::with_capacity(iterations);
    let mut combined_samples = Vec::with_capacity(iterations);
    let mut missed_deadlines = 0_usize;
    let mut over_budget = 0_usize;
    let mut checksum = 0_u64;
    let mut deadline = Instant::now();

    for _ in 0..iterations {
        deadline = deadline
            .checked_add(interval)
            .ok_or("live viewport deadline overflowed")?;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            missed_deadlines += 1;
            if job.progress().done {
                return live_finished_early(
                    job,
                    "indexing finished before the live viewport workload completed",
                );
            }
            continue;
        };
        thread::sleep(remaining);
        if job.progress().done {
            return live_finished_early(
                job,
                "indexing finished before the live viewport workload completed",
            );
        }

        let combined_started = Instant::now();
        let snapshot_started = Instant::now();
        let index = job.snapshot();
        let snapshot_elapsed = snapshot_started.elapsed();
        let start = combined_samples.len() as u64 * rows as u64;
        let read_started = Instant::now();
        let selected = session.read_rows(&index, start, rows)?;
        let read_elapsed = read_started.elapsed();
        let combined_elapsed = combined_started.elapsed();
        if job.progress().done {
            return live_finished_early(job, "indexing finished during the live viewport workload");
        }

        snapshot_samples.push(snapshot_elapsed);
        read_samples.push(read_elapsed);
        combined_samples.push(combined_elapsed);
        over_budget += usize::from(combined_elapsed > interval);
        checksum = selected.iter().fold(checksum, |checksum, row| {
            row.fields
                .iter()
                .fold(checksum ^ row.offset, |sum, field| sum ^ field.len() as u64)
        });
    }
    if combined_samples.is_empty() {
        return Err("every live viewport request deadline was missed".into());
    }

    let sampled = job.progress();
    while !job.progress().done {
        thread::sleep(Duration::from_millis(10));
    }
    let finished = job.progress();
    let index = job.wait()?;

    println!("Quarry live viewport benchmark\n");
    println!("File: {}", session.path().display());
    println!("File size: {}", human_bytes(session.file_size));
    println!("Cache state: {cache_state}");
    println!("Index chunk: {}", human_bytes(chunk_bytes as u64));
    println!("Scheduled requests: {iterations}, {rows} rows per viewport");
    println!("Completed requests: {}", combined_samples.len());
    println!(
        "Request interval: {:.3} ms",
        interval.as_secs_f64() * 1000.0
    );
    println!(
        "Sampling window: {:.2}% to {:.2}% indexed\n",
        ready.bytes_scanned as f64 * 100.0 / ready.file_size.max(1) as f64,
        sampled.bytes_scanned as f64 * 100.0 / sampled.file_size.max(1) as f64
    );
    println!("Pattern       min ms    p50 ms    p95 ms    max ms    service/s");
    print_viewport_stats("snapshot", latency_stats(&mut snapshot_samples));
    print_viewport_stats("row read", latency_stats(&mut read_samples));
    print_viewport_stats("combined", latency_stats(&mut combined_samples));
    println!("Missed request deadlines: {missed_deadlines} / {iterations}");
    println!(
        "Combined reads over {:.3} ms: {over_budget} / {}",
        interval.as_secs_f64() * 1000.0,
        combined_samples.len()
    );
    println!("Checksum: {checksum}");
    println!("Rows indexed: {}", index.indexed_rows());
    println!("Indexing time: {:.3} s", finished.elapsed.as_secs_f64());
    println!(
        "Indexing throughput: {}/s",
        human_bytes(rate(finished.bytes_scanned, finished.elapsed))
    );
    println!("Index memory: {}", human_bytes(index.memory_bytes() as u64));
    println!("Current memory: {}", optional_bytes(current_rss_bytes()));
    println!("Peak memory: {}", optional_bytes(peak_rss_bytes()));
    Ok(())
}

fn live_finished_early(job: IndexJob, message: &'static str) -> CliResult<()> {
    match job.wait() {
        Ok(_) => Err(message.into()),
        Err(error) => Err(error.into()),
    }
}

fn latency_stats(samples: &mut [Duration]) -> LatencyStats {
    samples.sort_unstable();
    let percentile = |value: usize| {
        let rank = samples.len().saturating_mul(value).div_ceil(100).max(1);
        samples[rank.min(samples.len()) - 1]
    };
    LatencyStats {
        min: samples[0],
        p50: percentile(50),
        p95: percentile(95),
        max: samples[samples.len() - 1],
        total: samples.iter().sum(),
        count: samples.len(),
    }
}

fn print_viewport_stats(pattern: &str, stats: LatencyStats) {
    let milliseconds = |duration: Duration| duration.as_secs_f64() * 1000.0;
    println!(
        "{pattern:<10} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>12.1}",
        milliseconds(stats.min),
        milliseconds(stats.p50),
        milliseconds(stats.p95),
        milliseconds(stats.max),
        stats.count as f64 / stats.total.as_secs_f64()
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
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        data_search_position, filter_command, generate_file, latency_stats, parse_size,
        physical_to_data_row, search_command, viewport_command,
    };

    #[test]
    fn summarizes_viewport_latency_and_rejects_empty_workloads() {
        let mut samples = [1, 2, 3, 4, 100].map(Duration::from_millis);
        let stats = latency_stats(&mut samples);
        assert_eq!(stats.min, Duration::from_millis(1));
        assert_eq!(stats.p50, Duration::from_millis(3));
        assert_eq!(stats.p95, Duration::from_millis(100));
        assert_eq!(stats.max, Duration::from_millis(100));
        assert_eq!(stats.total, Duration::from_millis(110));
        assert_eq!(stats.count, 5);

        let error = viewport_command(vec!["--iterations".into(), "0".into()]).unwrap_err();
        assert_eq!(error.to_string(), "iterations and rows must be non-zero");

        let error = viewport_command(vec!["--live".into(), "--interval-ms".into(), "0".into()])
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "interval and chunk bytes must be non-zero"
        );

        let error = viewport_command(vec!["--chunk-bytes".into(), "0".into()]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "interval and chunk bytes must be non-zero"
        );

        let error = viewport_command(vec![
            "--live".into(),
            "--iterations".into(),
            "1000".into(),
            "--interval-ms".into(),
            "1000".into(),
        ])
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "live viewport schedule must not exceed 60 seconds"
        );
    }

    #[test]
    fn validates_search_arguments_and_converts_data_coordinates() {
        let error = search_command(Vec::new()).unwrap_err();
        assert_eq!(error.to_string(), "search requires a file path");

        let error = search_command(vec!["missing.csv".into()]).unwrap_err();
        assert_eq!(error.to_string(), "search requires --query");

        for (option, expected) in [
            ("--start-row", "start row and column must be at least 1"),
            ("--start-column", "start row and column must be at least 1"),
            (
                "--cancel-after-bytes",
                "cancel-after-bytes must be non-zero",
            ),
        ] {
            let error = search_command(vec![
                "missing.csv".into(),
                "--query".into(),
                "needle".into(),
                option.into(),
                "0".into(),
            ])
            .unwrap_err();
            assert_eq!(error.to_string(), expected);
        }

        let error = search_command(vec!["missing.csv".into(), "--query".into(), String::new()])
            .unwrap_err();
        assert_eq!(error.to_string(), "query must not be empty");

        let error = search_command(vec![
            "missing.csv".into(),
            "--query".into(),
            "needle".into(),
            "--cache-state".into(),
            "hot".into(),
        ])
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "--cache-state must be unknown, cold, or warm"
        );

        let error = search_command(vec![
            "missing.csv".into(),
            "--query".into(),
            "needle".into(),
            "--cache-state".into(),
            "cold".into(),
        ])
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "search cannot be cold because indexing reads the file first"
        );

        let with_header = data_search_position(1, 1, true).unwrap();
        assert_eq!((with_header.row, with_header.column), (1, 0));
        assert_eq!(physical_to_data_row(with_header.row, true), 1);

        let without_header = data_search_position(42, 3, false).unwrap();
        assert_eq!((without_header.row, without_header.column), (41, 2));
        assert_eq!(physical_to_data_row(without_header.row, false), 42);

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "quarry-search-command-{}-{suffix}.csv",
            std::process::id()
        ));
        fs::write(&path, b"name,value\none,needle\n").unwrap();
        search_command(vec![
            path.to_string_lossy().into_owned(),
            "--query".into(),
            "needle".into(),
        ])
        .unwrap();

        let error = search_command(vec![
            path.to_string_lossy().into_owned(),
            "--query".into(),
            "missing".into(),
            "--cancel-after-bytes".into(),
            fs::metadata(&path).unwrap().len().to_string(),
        ])
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "cancel-after-bytes must be less than the searchable byte span"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn validates_filter_arguments_and_reads_bounded_samples() {
        let error = filter_command(Vec::new()).unwrap_err();
        assert_eq!(error.to_string(), "filter requires a file path");

        let error = filter_command(vec!["missing.csv".into()]).unwrap_err();
        assert_eq!(error.to_string(), "filter requires --column");

        for (args, expected) in [
            (
                vec![
                    "missing.csv",
                    "--column",
                    "0",
                    "--operator",
                    "equals",
                    "--value",
                    "x",
                ],
                "filter column must be at least 1",
            ),
            (
                vec![
                    "missing.csv",
                    "--column",
                    "1",
                    "--operator",
                    "contains",
                    "--value",
                    "",
                ],
                "contains filter value must not be empty",
            ),
            (
                vec![
                    "missing.csv",
                    "--column",
                    "1",
                    "--operator",
                    "regex",
                    "--value",
                    "x",
                ],
                "--operator must be contains or equals",
            ),
            (
                vec![
                    "missing.csv",
                    "--column",
                    "1",
                    "--operator",
                    "equals",
                    "--value",
                    "x",
                    "--cancel-after-bytes",
                    "0",
                ],
                "cancel-after-bytes must be non-zero",
            ),
            (
                vec![
                    "missing.csv",
                    "--column",
                    "1",
                    "--operator",
                    "equals",
                    "--value",
                    "x",
                    "--cache-state",
                    "hot",
                ],
                "--cache-state must be unknown, cold, or warm",
            ),
        ] {
            let error = filter_command(args.into_iter().map(str::to_owned).collect()).unwrap_err();
            assert_eq!(error.to_string(), expected);
        }

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "quarry-filter-command-{}-{suffix}.csv",
            std::process::id()
        ));
        fs::write(
            &path,
            b"name,note\none,\"line one\nline two\"\ntwo,needle\nthree,needle\nfour,\n",
        )
        .unwrap();

        for (operator, value) in [
            ("contains", "line one\nline two"),
            ("equals", "needle"),
            ("equals", ""),
        ] {
            filter_command(vec![
                path.to_string_lossy().into_owned(),
                "--column".into(),
                "2".into(),
                "--operator".into(),
                operator.into(),
                "--value".into(),
                value.into(),
            ])
            .unwrap();
        }

        let error = filter_command(vec![
            path.to_string_lossy().into_owned(),
            "--column".into(),
            "1".into(),
            "--operator".into(),
            "contains".into(),
            "--value".into(),
            "missing".into(),
            "--cancel-after-bytes".into(),
            fs::metadata(&path).unwrap().len().to_string(),
        ])
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "cancel-after-bytes must be less than file size"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filter_command_exercises_cancellation_branch() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "quarry-filter-cancellation-{}-{suffix}.csv",
            std::process::id()
        ));
        generate_file(&path, 64 * 1024 * 1024, 11, b',', 7).unwrap();
        filter_command(vec![
            path.to_string_lossy().into_owned(),
            "--column".into(),
            "1".into(),
            "--operator".into(),
            "contains".into(),
            "--value".into(),
            "QUARRY_NO_MATCH_9F7B2C".into(),
            "--cancel-after-bytes".into(),
            "1".into(),
        ])
        .unwrap();
        fs::remove_file(path).unwrap();
    }

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
