use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use quarry_core::{
    CaseSensitivity, ColumnTransformation, Dialect, FilterExportJob, FilterExportOutcome,
    FilterExportProgress, FilterIndex, FilterJob, FilterMatch, FilterOperator, FilterPredicate,
    FilterProgress, FilterQuery, HeaderMode, IndexConfig, IndexJob, IndexProgress,
    LiteralReplacement, MAX_TRANSFORMATION_COLUMNS, OpenOptions, ReplaceAllJob, ReplaceAllOutcome,
    SaveAsJob, SaveAsOutcome, SaveAsProgress, SearchJob, SearchOutcome, SearchPosition,
    SearchProgress, Session, SortDirection, SortJob, SortOutcome, SortProgress, SortSpec,
    SplitAnalysisJob, SplitAnalysisOutcome, SplitAnalysisProgress, StructuralIndex,
    estimate_sort_temporary_bytes,
};

type CliResult<T> = Result<T, Box<dyn Error>>;
type SearchRun = (SearchOutcome, SearchProgress, Option<(u64, Duration)>);
type FilterRun = (FilterIndex, FilterProgress, Option<(u64, Duration)>);
type FilterExportRun = (
    FilterExportOutcome,
    FilterExportProgress,
    Option<(u64, Duration)>,
);
type SaveAsRun = (SaveAsOutcome, SaveAsProgress, Option<(u64, Duration)>);
type ReplaceAllRun = (ReplaceAllOutcome, SaveAsProgress, Option<(u64, Duration)>);
type SortRun = (SortOutcome, SortProgress, Option<u64>);

const MAX_LIVE_BENCHMARK_MILLIS: u128 = 60_000;
const FILTER_SAMPLE_ROWS: usize = 100;
const RAW_HEADER_COMPARE_BUFFER_BYTES: usize = 64 * 1024;

pub fn run(args: impl IntoIterator<Item = String>) -> CliResult<()> {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        Some("open") => open_command(args.collect()),
        Some("viewport") => viewport_command(args.collect()),
        Some("search") => search_command(args.collect()),
        Some("filter") => filter_command(args.collect()),
        Some("export") => export_command(args.collect()),
        Some("edit-save-as") => edit_save_as_command(args.collect()),
        Some("replace-all-save-as") => replace_all_save_as_command(args.collect()),
        Some("transform-save-as") => transform_save_as_command(args.collect()),
        Some("sort-save-as") => sort_save_as_command(args.collect()),
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
         [--jump-count 5] [--cache-state unknown|cold|warm] [--metrics-only] [--no-wait]\n  \
           quarry viewport <FILE> [--iterations 500] [--rows 100] \
         [--seed 1] [--cache-state unknown|cold|warm] [--live] \
         [--interval-ms 16] [--chunk-bytes 1048576]\n  \
           quarry search <FILE> --query LITERAL [--start-row 1] \
         [--start-column 1] [--cancel-after-bytes N] \
         [--cache-state unknown|warm]\n  \
           quarry filter <FILE> --column N --operator contains|equals|not-equals \
         --value LITERAL [--and N contains|equals|not-equals LITERAL]... \
         [--cancel-after-bytes N] \
         [--cache-state unknown|cold|warm]\n  \
           quarry export <FILE> --output FILE --column N \
         --operator contains|equals|not-equals --value LITERAL \
         [--and N contains|equals|not-equals LITERAL]... [--cancel-after-bytes N] \
         [--cache-state unknown|cold|warm]\n  \
           quarry edit-save-as <FILE> --output FILE \
         --edit DATA_ROW COLUMN VALUE [--edit DATA_ROW COLUMN VALUE]... \
         [--cancel-after-bytes N] [--cache-state unknown|cold|warm]\n  \
           quarry replace-all-save-as <FILE> --output FILE --query LITERAL \
         --replacement LITERAL [--cancel-after-bytes N] \
         [--cache-state unknown|cold|warm]\n  \
           quarry transform-save-as <FILE> --output FILE \
         (--split COLUMN SEPARATOR OUTPUT_COUNT | --split-auto COLUMN SEPARATOR | \
         --join COLUMNS SEPARATOR) \
         [--output-header NAME]... [--cancel-after-bytes N] \
         [--cache-state unknown|cold|warm]\n  \
           quarry sort-save-as <SOURCE> <DESTINATION> --column N \
         --order asc|desc [--delimiter ,] [--header auto|first-row|none] \
         [--cancel-after-bytes N] [--cache-state unknown|cold|warm]\n  \
           quarry generate --size 10GB --columns 40 --delimiter , \
         --output FILE [--seed 1]"
    );
}

fn parse_filter_operator(value: &str, option: &str) -> CliResult<FilterOperator> {
    match value {
        "contains" => Ok(FilterOperator::Contains),
        "equals" => Ok(FilterOperator::Equals),
        "not-equals" => Ok(FilterOperator::NotEquals),
        _ => Err(format!("{option} must be contains, equals, or not-equals").into()),
    }
}

fn filter_command(args: Vec<String>) -> CliResult<()> {
    let mut path = None;
    let mut column = None;
    let mut operator = None;
    let mut filter_value = None;
    let mut and_predicates = Vec::new();
    let mut cancel_after_bytes = None;
    let mut cache_state = "unknown".to_owned();
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--column" => column = Some(value(&args, &mut cursor, "--column")?.parse::<usize>()?),
            "--operator" => {
                operator = Some(parse_filter_operator(
                    value(&args, &mut cursor, "--operator")?,
                    "--operator",
                )?)
            }
            "--value" => {
                filter_value = Some(value(&args, &mut cursor, "--value")?.as_bytes().to_vec())
            }
            "--and" => {
                let operands = args
                    .get(cursor + 1..cursor + 4)
                    .ok_or("--and requires COLUMN contains|equals|not-equals VALUE")?;
                let column = operands[0].parse::<usize>()?;
                let operator = parse_filter_operator(&operands[1], "--and operator")?;
                let value = operands[2].as_bytes().to_vec();
                if column == 0 {
                    return Err("AND filter column must be at least 1".into());
                }
                if operator == FilterOperator::Contains && value.is_empty() {
                    return Err("AND contains filter value must not be empty".into());
                }
                and_predicates.push(FilterPredicate {
                    column: column - 1,
                    operator,
                    value,
                });
                cursor += 3;
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
    let mut query = FilterQuery::single(column - 1, operator, filter_value);
    query.predicates.extend(and_predicates);
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
    println!("Predicate count: {}", index.query().predicates.len());
    for (position, predicate) in index.query().predicates.iter().enumerate() {
        println!(
            "Predicate {}: column {}, operator {}, value {}",
            position + 1,
            predicate.column + 1,
            match predicate.operator {
                FilterOperator::Contains => "contains",
                FilterOperator::Equals => "equals",
                FilterOperator::NotEquals => "not-equals",
            },
            render_field(&predicate.value)
        );
    }
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

fn export_command(args: Vec<String>) -> CliResult<()> {
    let mut path = None;
    let mut destination = None;
    let mut column = None;
    let mut operator = None;
    let mut filter_value = None;
    let mut and_predicates = Vec::new();
    let mut cancel_after_bytes = None;
    let mut cache_state = "unknown".to_owned();
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--output" => destination = Some(PathBuf::from(value(&args, &mut cursor, "--output")?)),
            "--column" => column = Some(value(&args, &mut cursor, "--column")?.parse::<usize>()?),
            "--operator" => {
                operator = Some(parse_filter_operator(
                    value(&args, &mut cursor, "--operator")?,
                    "--operator",
                )?)
            }
            "--value" => {
                filter_value = Some(value(&args, &mut cursor, "--value")?.as_bytes().to_vec())
            }
            "--and" => {
                let operands = args
                    .get(cursor + 1..cursor + 4)
                    .ok_or("--and requires COLUMN contains|equals|not-equals VALUE")?;
                let column = operands[0].parse::<usize>()?;
                let operator = parse_filter_operator(&operands[1], "--and operator")?;
                let value = operands[2].as_bytes().to_vec();
                if column == 0 {
                    return Err("AND filter column must be at least 1".into());
                }
                if operator == FilterOperator::Contains && value.is_empty() {
                    return Err("AND contains filter value must not be empty".into());
                }
                and_predicates.push(FilterPredicate {
                    column: column - 1,
                    operator,
                    value,
                });
                cursor += 3;
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

    let path = path.ok_or("export requires a file path")?;
    let destination = destination.ok_or("export requires --output")?;
    let column = column.ok_or("export requires --column")?;
    let operator = operator.ok_or("export requires --operator")?;
    let filter_value = filter_value.ok_or("export requires --value")?;
    if column == 0 {
        return Err("export column must be at least 1".into());
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
    let mut query = FilterQuery::single(column - 1, operator, filter_value);
    query.predicates.extend(and_predicates);
    let source_size_before = session.file_size;
    let job = session.start_filtered_export(query.clone(), &destination)?;
    let (outcome, progress, cancellation) =
        wait_for_export(job, cancel_after_bytes, Duration::from_millis(1))?;
    let source_size_after = std::fs::metadata(session.path())?.len();
    if source_size_after != source_size_before {
        return Err("source file size changed during export".into());
    }

    let (outcome_label, published_rows, published_bytes) = match &outcome {
        FilterExportOutcome::Complete(summary) => {
            let output_size = std::fs::metadata(&summary.destination)?.len();
            if summary.rows_written != progress.rows_written
                || summary.bytes_written != progress.bytes_written
                || output_size != summary.bytes_written
            {
                return Err("published output does not match export progress".into());
            }
            ("complete", Some(summary.rows_written), Some(output_size))
        }
        FilterExportOutcome::Cancelled => {
            if destination.exists() {
                return Err("cancelled export published a destination file".into());
            }
            ("cancelled", None, None)
        }
    };

    println!("Quarry filtered export benchmark\n");
    println!("Source: {}", session.path().display());
    println!(
        "Source size: {} ({} bytes)",
        human_bytes(source_size_before),
        source_size_before
    );
    println!("Destination: {}", destination.display());
    println!(
        "Build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("Export cache state: {cache_state}");
    println!("Predicate count: {}", query.predicates.len());
    for (position, predicate) in query.predicates.iter().enumerate() {
        println!(
            "Predicate {}: column {}, operator {}, value {}",
            position + 1,
            predicate.column + 1,
            match predicate.operator {
                FilterOperator::Contains => "contains",
                FilterOperator::Equals => "equals",
                FilterOperator::NotEquals => "not-equals",
            },
            render_field(&predicate.value)
        );
    }
    println!("Outcome: {outcome_label}");
    println!(
        "Bytes scanned: {} ({} bytes) of {} ({} bytes)",
        human_bytes(progress.bytes_scanned),
        progress.bytes_scanned,
        human_bytes(progress.total_bytes),
        progress.total_bytes
    );
    println!("Physical records scanned: {}", progress.rows_scanned);
    println!("Matching rows written: {}", progress.rows_written);
    println!(
        "Output bytes written: {} ({} bytes)",
        human_bytes(progress.bytes_written),
        progress.bytes_written
    );
    println!("Export time: {:.3} s", progress.elapsed.as_secs_f64());
    println!(
        "Scan throughput: {}/s",
        human_bytes(rate(progress.bytes_scanned, progress.elapsed))
    );
    println!(
        "Output throughput: {}/s",
        human_bytes(rate(progress.bytes_written, progress.elapsed))
    );
    println!("Source size unchanged: yes ({source_size_after} bytes)");
    println!(
        "Destination published: {}",
        if published_bytes.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    if let (Some(rows), Some(bytes)) = (published_rows, published_bytes) {
        println!("Published rows: {rows}");
        println!("Published size: {} ({bytes} bytes)", human_bytes(bytes));
    }
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
        "Peak process memory (export): {}",
        optional_bytes(peak_rss_bytes())
    );
    Ok(())
}

fn wait_for_export(
    job: FilterExportJob,
    cancel_after_bytes: Option<u64>,
    poll_interval: Duration,
) -> CliResult<FilterExportRun> {
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
        return Err("export finished before cancellation threshold".into());
    }
    if cancel_after_bytes.is_some() && !matches!(&outcome, FilterExportOutcome::Cancelled) {
        return Err("export completed before cancellation took effect".into());
    }
    if cancel_after_bytes.is_some() && progress.bytes_scanned >= progress.total_bytes {
        return Err("export reached end of file before cancellation took effect".into());
    }
    Ok((outcome, progress, cancellation))
}

#[derive(Debug, Clone)]
struct RequestedCellEdit {
    data_row: u64,
    column: usize,
    value: Vec<u8>,
}

fn edit_save_as_command(args: Vec<String>) -> CliResult<()> {
    let mut path = None;
    let mut destination = None;
    let mut requested_edits = Vec::new();
    let mut cancel_after_bytes = None;
    let mut cache_state = "unknown".to_owned();
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--output" => destination = Some(PathBuf::from(value(&args, &mut cursor, "--output")?)),
            "--edit" => {
                let operands = args
                    .get(cursor + 1..cursor + 4)
                    .ok_or("--edit requires DATA_ROW COLUMN VALUE")?;
                let data_row = operands[0].parse::<u64>()?;
                let column = operands[1].parse::<usize>()?;
                if data_row == 0 || column == 0 {
                    return Err("edit data row and column must be at least 1".into());
                }
                requested_edits.push(RequestedCellEdit {
                    data_row,
                    column,
                    value: operands[2].as_bytes().to_vec(),
                });
                cursor += 3;
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
            argument if path.is_none() => path = Some(PathBuf::from(argument)),
            argument => return Err(format!("unexpected argument {argument:?}").into()),
        }
        cursor += 1;
    }

    let path = path.ok_or("edit-save-as requires a file path")?;
    let destination = destination.ok_or("edit-save-as requires --output")?;
    if requested_edits.is_empty() {
        return Err("edit-save-as requires at least one --edit".into());
    }
    if cancel_after_bytes == Some(0) {
        return Err("cancel-after-bytes must be non-zero".into());
    }

    let session = Session::open(&path, OpenOptions::default())?;
    if cancel_after_bytes.is_some_and(|bytes| bytes >= session.file_size) {
        return Err("cancel-after-bytes must be less than file size".into());
    }
    let source_size_before = session.file_size;
    let mut cell_edits = BTreeMap::new();
    for requested in &requested_edits {
        let row = requested
            .data_row
            .checked_sub(1)
            .and_then(|row| row.checked_add(u64::from(session.dialect.has_header)))
            .ok_or("edit position is out of range")?;
        let column = requested.column - 1;
        if cell_edits
            .insert((row, column), requested.value.clone())
            .is_some()
        {
            return Err(format!(
                "duplicate edit for data row {}, column {}",
                requested.data_row, requested.column
            )
            .into());
        }
    }

    let job =
        session.start_save_as_with_edits(BTreeMap::new(), cell_edits.clone(), &destination)?;
    let (outcome, progress, cancellation) =
        wait_for_save_as(job, cancel_after_bytes, Duration::from_millis(1))?;
    let source_size_after = std::fs::metadata(session.path())?.len();
    if source_size_after != source_size_before {
        return Err("source file size changed during Save As".into());
    }
    let save_peak_rss = peak_rss_bytes();
    let save_current_rss = current_rss_bytes();

    let (outcome_label, published_bytes, validated_edits, validation_elapsed) = match &outcome {
        SaveAsOutcome::Complete(summary) => {
            let output_size = std::fs::metadata(&summary.destination)?.len();
            if summary.bytes_written != progress.bytes_written
                || output_size != summary.bytes_written
            {
                return Err("published output does not match Save As progress".into());
            }
            let validation_elapsed =
                validate_saved_edits(&summary.destination, &cell_edits, session.dialect)?;
            (
                "complete",
                Some(output_size),
                Some(cell_edits.len()),
                Some(validation_elapsed),
            )
        }
        SaveAsOutcome::Cancelled => {
            if destination.exists() {
                return Err("cancelled Save As published a destination file".into());
            }
            ("cancelled", None, None, None)
        }
    };

    println!("Quarry direct cell Save As benchmark\n");
    println!("Source: {}", session.path().display());
    println!(
        "Source size: {} ({} bytes)",
        human_bytes(source_size_before),
        source_size_before
    );
    println!("Destination: {}", destination.display());
    println!(
        "Build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("Save As cache state: {cache_state}");
    println!("Sparse cell edits: {}", requested_edits.len());
    for (position, edit) in requested_edits.iter().enumerate() {
        println!(
            "Edit {}: data row {}, column {}, value {}",
            position + 1,
            edit.data_row,
            edit.column,
            render_field(&edit.value)
        );
    }
    println!("Outcome: {outcome_label}");
    println!(
        "Bytes scanned: {} ({} bytes) of {} ({} bytes)",
        human_bytes(progress.bytes_scanned),
        progress.bytes_scanned,
        human_bytes(progress.total_bytes),
        progress.total_bytes
    );
    println!(
        "Output bytes written: {} ({} bytes)",
        human_bytes(progress.bytes_written),
        progress.bytes_written
    );
    println!("Save As time: {:.3} s", progress.elapsed.as_secs_f64());
    println!(
        "Scan throughput: {}/s",
        human_bytes(rate(progress.bytes_scanned, progress.elapsed))
    );
    println!(
        "Output throughput: {}/s",
        human_bytes(rate(progress.bytes_written, progress.elapsed))
    );
    println!("Source size unchanged: yes ({source_size_after} bytes)");
    println!(
        "Destination published: {}",
        if published_bytes.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    if let Some(bytes) = published_bytes {
        println!("Published size: {} ({bytes} bytes)", human_bytes(bytes));
    }
    if let (Some(edits), Some(elapsed)) = (validated_edits, validation_elapsed) {
        println!("Validated edited cells: {edits}");
        println!("Validation index and reads: {:.3} s", elapsed.as_secs_f64());
    }
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
        "Current process memory after Save As: {}",
        optional_bytes(save_current_rss)
    );
    println!(
        "Peak process memory through Save As: {}",
        optional_bytes(save_peak_rss)
    );
    Ok(())
}

fn replace_all_save_as_command(args: Vec<String>) -> CliResult<()> {
    let mut path = None;
    let mut destination = None;
    let mut query = None;
    let mut replacement = None;
    let mut cancel_after_bytes = None;
    let mut cache_state = "unknown".to_owned();
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--output" => destination = Some(PathBuf::from(value(&args, &mut cursor, "--output")?)),
            "--query" => query = Some(value(&args, &mut cursor, "--query")?.as_bytes().to_vec()),
            "--replacement" => {
                replacement = Some(
                    value(&args, &mut cursor, "--replacement")?
                        .as_bytes()
                        .to_vec(),
                )
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
            argument if path.is_none() => path = Some(PathBuf::from(argument)),
            argument => return Err(format!("unexpected argument {argument:?}").into()),
        }
        cursor += 1;
    }

    let path = path.ok_or("replace-all-save-as requires a file path")?;
    let destination = destination.ok_or("replace-all-save-as requires --output")?;
    let query = query.ok_or("replace-all-save-as requires --query")?;
    let replacement = replacement.ok_or("replace-all-save-as requires --replacement")?;
    if query.is_empty() {
        return Err("replace-all query must not be empty".into());
    }
    if query == replacement {
        return Err("replace-all query and replacement must differ".into());
    }
    if cancel_after_bytes == Some(0) {
        return Err("cancel-after-bytes must be non-zero".into());
    }

    let session = Session::open(&path, OpenOptions::default())?;
    if cancel_after_bytes.is_some_and(|bytes| bytes >= session.file_size) {
        return Err("cancel-after-bytes must be less than file size".into());
    }
    let source_size_before = session.file_size;
    let job = session.start_create_replaced_working_copy(
        BTreeMap::new(),
        BTreeMap::new(),
        LiteralReplacement {
            needle: query.clone(),
            replacement: replacement.clone(),
            case_sensitivity: CaseSensitivity::Sensitive,
        },
        &destination,
    )?;
    let (outcome, progress, cancellation) =
        wait_for_replace_all(job, cancel_after_bytes, Duration::from_millis(1))?;
    let source_size_after = std::fs::metadata(session.path())?.len();
    if source_size_after != source_size_before {
        return Err("source file size changed during Replace All".into());
    }
    let replace_peak_rss = peak_rss_bytes();
    let replace_current_rss = current_rss_bytes();

    let (outcome_label, published_bytes, replacements) = match &outcome {
        ReplaceAllOutcome::Complete(summary) => {
            let output_size = std::fs::metadata(&summary.destination)?.len();
            if summary.bytes_written != progress.bytes_written
                || output_size != summary.bytes_written
                || summary.replacements == 0
            {
                return Err("published output does not match Replace All progress".into());
            }
            ("complete", Some(output_size), summary.replacements)
        }
        ReplaceAllOutcome::NoMatch => {
            if destination.exists() {
                return Err("no-match Replace All published a destination file".into());
            }
            ("no match", None, 0)
        }
        ReplaceAllOutcome::Cancelled => {
            if destination.exists() {
                return Err("cancelled Replace All published a destination file".into());
            }
            ("cancelled", None, 0)
        }
    };

    println!("Quarry Replace All Save As benchmark\n");
    println!("Source: {}", session.path().display());
    println!(
        "Source size: {} ({} bytes)",
        human_bytes(source_size_before),
        source_size_before
    );
    println!("Destination: {}", destination.display());
    println!(
        "Build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("Replace All cache state: {cache_state}");
    println!("Query: {}", render_field(&query));
    println!("Replacement: {}", render_field(&replacement));
    println!("Outcome: {outcome_label}");
    println!("Replacements: {replacements}");
    println!(
        "Bytes scanned: {} ({} bytes) of {} ({} bytes)",
        human_bytes(progress.bytes_scanned),
        progress.bytes_scanned,
        human_bytes(progress.total_bytes),
        progress.total_bytes
    );
    println!(
        "Output bytes written: {} ({} bytes)",
        human_bytes(progress.bytes_written),
        progress.bytes_written
    );
    println!("Replace All time: {:.3} s", progress.elapsed.as_secs_f64());
    println!(
        "Scan throughput: {}/s",
        human_bytes(rate(progress.bytes_scanned, progress.elapsed))
    );
    println!(
        "Output throughput: {}/s",
        human_bytes(rate(progress.bytes_written, progress.elapsed))
    );
    println!("Source size unchanged: yes ({source_size_after} bytes)");
    println!(
        "Destination published: {}",
        if published_bytes.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    if let Some(bytes) = published_bytes {
        println!("Published size: {} ({bytes} bytes)", human_bytes(bytes));
    }
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
        "Current process memory after Replace All: {}",
        optional_bytes(replace_current_rss)
    );
    println!(
        "Peak process memory through Replace All: {}",
        optional_bytes(replace_peak_rss)
    );
    Ok(())
}

#[derive(Debug, Clone)]
enum RequestedColumnTransformation {
    Split {
        source_column: usize,
        separator: Vec<u8>,
        output_count: usize,
    },
    SplitAuto {
        source_column: usize,
        separator: Vec<u8>,
    },
    Join {
        source_columns: Vec<usize>,
        separator: Vec<u8>,
    },
}

fn sort_save_as_command(args: Vec<String>) -> CliResult<()> {
    let mut source = None;
    let mut destination = None;
    let mut column = None;
    let mut direction = None;
    let mut delimiter = None;
    let mut header_mode = HeaderMode::Auto;
    let mut cancel_after_bytes = None;
    let mut cache_state = "unknown".to_owned();
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--column" => column = Some(value(&args, &mut cursor, "--column")?.parse::<usize>()?),
            "--order" => {
                direction = Some(parse_sort_direction(value(&args, &mut cursor, "--order")?)?)
            }
            "--delimiter" => {
                delimiter = Some(parse_delimiter(value(&args, &mut cursor, "--delimiter")?)?)
            }
            "--header" => header_mode = parse_header_mode(value(&args, &mut cursor, "--header")?)?,
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
            argument if source.is_none() => source = Some(PathBuf::from(argument)),
            argument if destination.is_none() => destination = Some(PathBuf::from(argument)),
            argument => return Err(format!("unexpected argument {argument:?}").into()),
        }
        cursor += 1;
    }

    let source = source.ok_or("sort-save-as requires a source file path")?;
    let destination = destination.ok_or("sort-save-as requires a destination file path")?;
    let column = column.ok_or("sort-save-as requires --column")?;
    if column == 0 {
        return Err("sort column must be at least 1".into());
    }
    let direction = direction.ok_or("sort-save-as requires --order")?;
    if cancel_after_bytes == Some(0) {
        return Err("cancel-after-bytes must be non-zero".into());
    }

    let session = Session::open(
        &source,
        OpenOptions {
            delimiter,
            header_mode,
            ..OpenOptions::default()
        },
    )?;
    if cancel_after_bytes.is_some_and(|bytes| bytes >= session.file_size) {
        return Err("cancel-after-bytes must be less than file size".into());
    }
    let source_index = session.start_indexing(IndexConfig::default())?.wait()?;
    let spec = SortSpec {
        column: column - 1,
        direction,
        case_sensitivity: CaseSensitivity::Sensitive,
    };
    let source_size_before = session.file_size;
    let data_rows = source_index
        .indexed_rows()
        .saturating_sub(u64::from(session.dialect.has_header));
    let estimated_temporary_bytes = estimate_sort_temporary_bytes(
        source_size_before.saturating_add(data_rows.saturating_mul(2)),
        data_rows,
    );
    let job = session.start_create_sorted_working_copy(
        BTreeMap::new(),
        BTreeMap::new(),
        spec,
        &destination,
    )?;
    let (outcome, progress, cancellation_requested_at) =
        wait_for_sort(job, cancel_after_bytes, Duration::from_millis(1))?;
    let sort_peak_rss = peak_rss_bytes();
    let sort_current_rss = current_rss_bytes();
    let source_size_after = std::fs::metadata(session.path())?.len();
    if source_size_after != source_size_before {
        return Err("source file size changed during sort".into());
    }
    let source_hash = fnv1a64_file(session.path())?;

    let (outcome_label, published_bytes, validation, output_hash) = match &outcome {
        SortOutcome::Complete(summary) => {
            let output_size = std::fs::metadata(&summary.destination)?.len();
            if summary.destination != destination
                || summary.rows_sorted != progress.rows_sorted
                || summary.runs_created != progress.runs_created
                || summary.bytes_written != progress.bytes_written
                || summary.peak_temporary_bytes != progress.peak_temporary_bytes
                || summary.merge_passes != progress.merge_passes
                || summary.header_rows != progress.header_rows
                || summary.elapsed != progress.elapsed
                || output_size != summary.bytes_written
            {
                return Err("published output does not match sort progress".into());
            }
            let validation =
                validate_sorted_output(&session, &source_index, &summary.destination, spec)?;
            if validation.data_rows != summary.rows_sorted
                || summary.header_rows != u64::from(session.dialect.has_header)
            {
                return Err("sort summary does not preserve exact row/header counts".into());
            }
            validate_sort_completion_evidence(
                summary.record_multiset_verified,
                summary.stable_ties_verified,
            )?;
            (
                "complete",
                Some(output_size),
                Some(validation),
                Some(fnv1a64_file(&summary.destination)?),
            )
        }
        SortOutcome::Cancelled => {
            if destination.exists() {
                return Err("cancelled sort published a destination file".into());
            }
            ("cancelled", None, None, None)
        }
    };
    let completion_evidence = matches!(&outcome, SortOutcome::Complete(_));
    let artifact_permissions =
        sort_artifact_permissions(published_bytes.map(|_| destination.as_path()))?;

    println!("Quarry guarded sort validation artifact\n");
    println!("Source: {}", session.path().display());
    println!(
        "Source size: {} ({} bytes)",
        human_bytes(source_size_before),
        source_size_before
    );
    println!("Destination: {}", destination.display());
    println!("Artifact permissions: {artifact_permissions}");
    println!(
        "Delimiter: {}",
        display_delimiter(session.dialect.delimiter)
    );
    println!(
        "Header: {}",
        if session.dialect.has_header {
            "first row"
        } else {
            "none"
        }
    );
    println!("Cache state: {cache_state}");
    println!("Sort column: {column}");
    println!(
        "Sort order: {}",
        match direction {
            SortDirection::Ascending => "ascending",
            SortDirection::Descending => "descending",
        }
    );
    println!(
        "Estimated temporary disk: {} ({} bytes)",
        human_bytes(estimated_temporary_bytes),
        estimated_temporary_bytes
    );
    println!(
        "Peak temporary disk: {} ({} bytes)",
        human_bytes(progress.peak_temporary_bytes),
        progress.peak_temporary_bytes
    );
    println!("Outcome: {outcome_label}");
    println!("Rows sorted: {}", progress.rows_sorted);
    println!("Header rows: {}", progress.header_rows);
    println!("Sorted runs created: {}", progress.runs_created);
    println!("Merge passes: {}", progress.merge_passes);
    println!(
        "Bytes scanned: {} ({} bytes) of {} ({} bytes)",
        human_bytes(progress.bytes_scanned),
        progress.bytes_scanned,
        human_bytes(progress.total_bytes),
        progress.total_bytes
    );
    println!(
        "Output bytes written: {} ({} bytes)",
        human_bytes(progress.bytes_written),
        progress.bytes_written
    );
    println!("Sort wall time: {:.3} s", progress.elapsed.as_secs_f64());
    println!("Source size unchanged: yes ({source_size_after} bytes)");
    println!("Source FNV-1a 64: {source_hash:016x}");
    println!(
        "Destination published: {}",
        if published_bytes.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    if let Some(bytes) = published_bytes {
        println!("Published size: {} ({bytes} bytes)", human_bytes(bytes));
    }
    if let Some(hash) = output_hash {
        println!("Output FNV-1a 64: {hash:016x}");
    }
    if let Some(validation) = validation {
        println!(
            "Exact data row count preserved: yes ({})",
            validation.data_rows
        );
        if let Some(header_bytes) = validation.header_bytes {
            println!("Exact raw header preserved: yes ({header_bytes} bytes)");
        } else {
            println!("Exact raw header preserved: n/a (headerless source)");
        }
        println!(
            "Validation index and reads: {:.3} s",
            validation.elapsed.as_secs_f64()
        );
    }
    if completion_evidence {
        println!("Bounded record multiset evidence: verified (dual fingerprint)");
        println!("Stable equal-key order evidence: verified (source ordinals)");
    }
    if let Some(requested_at) = cancellation_requested_at {
        println!(
            "Cancellation requested after: {} ({} bytes)",
            human_bytes(requested_at),
            requested_at
        );
    }
    if let Some(latency) = progress.cancellation_latency {
        println!(
            "Cancellation latency: {:.3} ms",
            latency.as_secs_f64() * 1000.0
        );
    }
    println!(
        "Current process memory after sort: {}",
        optional_bytes(sort_current_rss)
    );
    println!(
        "Peak process RSS through sort: {}",
        optional_bytes(sort_peak_rss)
    );
    Ok(())
}

fn parse_sort_direction(value: &str) -> CliResult<SortDirection> {
    match value {
        "asc" => Ok(SortDirection::Ascending),
        "desc" => Ok(SortDirection::Descending),
        _ => Err("--order must be asc or desc".into()),
    }
}

fn parse_header_mode(value: &str) -> CliResult<HeaderMode> {
    match value {
        "auto" => Ok(HeaderMode::Auto),
        "first-row" => Ok(HeaderMode::FirstRow),
        "none" => Ok(HeaderMode::NoHeader),
        _ => Err("--header must be auto, first-row, or none".into()),
    }
}

fn validate_sort_completion_evidence(
    record_multiset_verified: bool,
    stable_ties_verified: bool,
) -> CliResult<()> {
    if !record_multiset_verified {
        return Err("sort did not verify record multiset preservation".into());
    }
    if !stable_ties_verified {
        return Err("sort did not verify stable equal-key ordering".into());
    }
    Ok(())
}

fn wait_for_sort(
    job: SortJob,
    cancel_after_bytes: Option<u64>,
    poll_interval: Duration,
) -> CliResult<SortRun> {
    let mut cancellation = None;
    loop {
        let progress = job.progress();
        if progress.done {
            break;
        }
        if cancellation.is_none()
            && cancel_after_bytes.is_some_and(|threshold| progress.bytes_scanned >= threshold)
        {
            job.cancel();
            cancellation = Some(progress.bytes_scanned);
        }
        thread::sleep(poll_interval);
    }

    let progress = job.progress();
    let outcome = job.wait()?;
    if cancel_after_bytes.is_some() && cancellation.is_none() {
        return Err("sort finished before cancellation threshold".into());
    }
    if cancel_after_bytes.is_some() && !matches!(&outcome, SortOutcome::Cancelled) {
        return Err("sort completed before cancellation took effect".into());
    }
    if cancel_after_bytes.is_some()
        && (!progress.cancelled || progress.cancellation_latency.is_none())
    {
        return Err("sort did not report completed cancellation metrics".into());
    }
    if cancel_after_bytes.is_some() && progress.bytes_scanned >= progress.total_bytes {
        return Err("sort reached end of file before cancellation took effect".into());
    }
    Ok((outcome, progress, cancellation))
}

struct SortValidation {
    data_rows: u64,
    header_bytes: Option<u64>,
    elapsed: Duration,
}

fn sort_artifact_permissions(path: Option<&Path>) -> CliResult<String> {
    let Some(path) = path else {
        return Ok("n/a (not published)".to_owned());
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        Ok(format!("{mode:04o} (observed Unix mode)"))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok("n/a (Unix mode unavailable)".to_owned())
    }
}

fn validate_sorted_output(
    source: &Session,
    source_index: &StructuralIndex,
    destination: &Path,
    spec: SortSpec,
) -> CliResult<SortValidation> {
    const VALIDATION_ROWS: usize = 1_000;

    let started = Instant::now();
    let output = Session::open(
        destination,
        OpenOptions {
            rows: 1,
            delimiter: Some(source.dialect.delimiter),
            header_mode: if source.dialect.has_header {
                HeaderMode::FirstRow
            } else {
                HeaderMode::NoHeader
            },
            ..OpenOptions::default()
        },
    )?;
    let output_index = output.start_indexing(IndexConfig::default())?.wait()?;
    if output_index.indexed_rows() != source_index.indexed_rows() {
        return Err("sorted output record count changed".into());
    }
    let header_bytes = compare_raw_headers(
        source.path(),
        source,
        source_index,
        destination,
        &output,
        &output_index,
    )?;

    let data_start = u64::from(source.dialect.has_header);
    let mut next_row = data_start;
    let mut previous_key: Option<Vec<u8>> = None;
    while next_row < output_index.indexed_rows() {
        let remaining = output_index.indexed_rows() - next_row;
        let rows = output.read_rows(
            &output_index,
            next_row,
            remaining.min(VALIDATION_ROWS as u64) as usize,
        )?;
        if rows.is_empty() {
            return Err("sorted output validation row is missing".into());
        }
        for row in &rows {
            let key = row.fields.get(spec.column).cloned().unwrap_or_default();
            if previous_key
                .as_ref()
                .is_some_and(|previous| match spec.direction {
                    SortDirection::Ascending => previous > &key,
                    SortDirection::Descending => previous < &key,
                })
            {
                return Err("sorted output is out of order".into());
            }
            previous_key = Some(key);
        }
        next_row += rows.len() as u64;
    }
    Ok(SortValidation {
        data_rows: next_row - data_start,
        header_bytes,
        elapsed: started.elapsed(),
    })
}

fn compare_raw_headers(
    source_path: &Path,
    source: &Session,
    source_index: &StructuralIndex,
    destination_path: &Path,
    destination: &Session,
    destination_index: &StructuralIndex,
) -> CliResult<Option<u64>> {
    let source_end = raw_header_end(source, source_index)?;
    let destination_end = raw_header_end(destination, destination_index)?;
    let (Some(source_end), Some(destination_end)) = (source_end, destination_end) else {
        return if source_end.is_none() && destination_end.is_none() {
            Ok(None)
        } else {
            Err("sorted output raw header changed".into())
        };
    };
    if source_end != destination_end {
        return Err("sorted output raw header changed".into());
    }

    let mut source_file = File::open(source_path)?;
    let mut destination_file = File::open(destination_path)?;
    let mut source_buffer = [0_u8; RAW_HEADER_COMPARE_BUFFER_BYTES];
    let mut destination_buffer = [0_u8; RAW_HEADER_COMPARE_BUFFER_BYTES];
    let mut remaining = source_end;
    while remaining > 0 {
        let length = usize::try_from(remaining.min(RAW_HEADER_COMPARE_BUFFER_BYTES as u64))?;
        source_file.read_exact(&mut source_buffer[..length])?;
        destination_file.read_exact(&mut destination_buffer[..length])?;
        if source_buffer[..length] != destination_buffer[..length] {
            return Err("sorted output raw header changed".into());
        }
        remaining -= length as u64;
    }
    Ok(Some(source_end))
}

fn raw_header_end(session: &Session, index: &StructuralIndex) -> CliResult<Option<u64>> {
    if !session.dialect.has_header {
        return Ok(None);
    }
    let rows = session.read_rows(index, 0, 2)?;
    if rows.is_empty() {
        return Err("headered source does not contain a header row".into());
    }
    let end = rows.get(1).map_or(session.file_size, |row| row.offset);
    Ok(Some(end))
}

fn fnv1a64_file(path: &Path) -> CliResult<u64> {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut hash = OFFSET;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hash);
        }
        for byte in &buffer[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
}

fn transform_save_as_command(args: Vec<String>) -> CliResult<()> {
    let mut path = None;
    let mut destination = None;
    let mut requested = None;
    let mut output_headers = Vec::new();
    let mut cancel_after_bytes = None;
    let mut cache_state = "unknown".to_owned();
    let mut cursor = 0;

    while cursor < args.len() {
        match args[cursor].as_str() {
            "--output" => destination = Some(PathBuf::from(value(&args, &mut cursor, "--output")?)),
            "--split" => {
                if requested.is_some() {
                    return Err(
                        "transform-save-as requires exactly one --split, --split-auto, or --join"
                            .into(),
                    );
                }
                let operands = args
                    .get(cursor + 1..cursor + 4)
                    .ok_or("--split requires COLUMN SEPARATOR OUTPUT_COUNT")?;
                let source_column = operands[0].parse::<usize>()?;
                let output_count = operands[2].parse::<usize>()?;
                if source_column == 0 {
                    return Err("split column must be at least 1".into());
                }
                if operands[1].is_empty() {
                    return Err("split separator must not be empty".into());
                }
                if output_count < 2 {
                    return Err("split output count must be at least 2".into());
                }
                if source_column > MAX_TRANSFORMATION_COLUMNS
                    || output_count > MAX_TRANSFORMATION_COLUMNS
                {
                    return Err("split columns exceed the supported limit".into());
                }
                requested = Some(RequestedColumnTransformation::Split {
                    source_column: source_column - 1,
                    separator: operands[1].as_bytes().to_vec(),
                    output_count,
                });
                cursor += 3;
            }
            "--split-auto" => {
                if requested.is_some() {
                    return Err(
                        "transform-save-as requires exactly one --split, --split-auto, or --join"
                            .into(),
                    );
                }
                let operands = args
                    .get(cursor + 1..cursor + 3)
                    .ok_or("--split-auto requires COLUMN SEPARATOR")?;
                let source_column = operands[0].parse::<usize>()?;
                if source_column == 0 {
                    return Err("split column must be at least 1".into());
                }
                if operands[1].is_empty() {
                    return Err("split separator must not be empty".into());
                }
                if source_column > MAX_TRANSFORMATION_COLUMNS {
                    return Err("split columns exceed the supported limit".into());
                }
                requested = Some(RequestedColumnTransformation::SplitAuto {
                    source_column: source_column - 1,
                    separator: operands[1].as_bytes().to_vec(),
                });
                cursor += 2;
            }
            "--join" => {
                if requested.is_some() {
                    return Err(
                        "transform-save-as requires exactly one --split, --split-auto, or --join"
                            .into(),
                    );
                }
                let operands = args
                    .get(cursor + 1..cursor + 3)
                    .ok_or("--join requires COLUMNS SEPARATOR")?;
                requested = Some(RequestedColumnTransformation::Join {
                    source_columns: parse_join_columns(&operands[0])?,
                    separator: operands[1].as_bytes().to_vec(),
                });
                cursor += 2;
            }
            "--output-header" => output_headers.push(
                value(&args, &mut cursor, "--output-header")?
                    .as_bytes()
                    .to_vec(),
            ),
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
            argument if path.is_none() => path = Some(PathBuf::from(argument)),
            argument => return Err(format!("unexpected argument {argument:?}").into()),
        }
        cursor += 1;
    }

    let path = path.ok_or("transform-save-as requires a file path")?;
    let destination = destination.ok_or("transform-save-as requires --output")?;
    let mut requested = requested
        .ok_or("transform-save-as requires exactly one --split, --split-auto, or --join")?;
    if cancel_after_bytes == Some(0) {
        return Err("cancel-after-bytes must be non-zero".into());
    }

    let session = Session::open(&path, OpenOptions::default())?;
    if cancel_after_bytes.is_some_and(|bytes| bytes >= session.file_size) {
        return Err("cancel-after-bytes must be less than file size".into());
    }
    let mut split_analysis = None;
    let mut auto_split = false;
    let mut auto_source_header = None;
    if let RequestedColumnTransformation::SplitAuto {
        source_column,
        separator,
    } = &requested
    {
        let source_column = *source_column;
        let separator = separator.clone();
        let source_width = session
            .first_rows
            .iter()
            .map(|row| row.fields.len())
            .max()
            .unwrap_or_default()
            .max(source_column + 1);
        let max_pieces = MAX_TRANSFORMATION_COLUMNS
            .saturating_sub(source_width)
            .saturating_add(1);
        if max_pieces < 2 {
            return Err("splitting would exceed the supported column limit".into());
        }
        let job = session.start_analyze_split(
            BTreeMap::new(),
            source_column,
            separator.clone(),
            max_pieces,
        )?;
        let (outcome, progress) = wait_for_split_analysis(job)?;
        let summary = match outcome {
            SplitAnalysisOutcome::Complete(summary) if summary.max_pieces >= 2 => summary,
            SplitAnalysisOutcome::Complete(_) => {
                return Err("split separator was not found in the selected column".into());
            }
            SplitAnalysisOutcome::Cancelled => {
                return Err("split analysis was cancelled".into());
            }
        };
        auto_source_header = session.dialect.has_header.then(|| {
            session
                .first_rows
                .first()
                .and_then(|row| row.fields.get(source_column))
                .cloned()
                .unwrap_or_default()
        });
        requested = RequestedColumnTransformation::Split {
            source_column,
            separator,
            output_count: summary.max_pieces,
        };
        split_analysis = Some((progress, summary.rows_scanned, summary.max_pieces));
        auto_split = true;
    }
    let transformation = if auto_split && output_headers.is_empty() {
        let RequestedColumnTransformation::Split {
            source_column,
            separator,
            output_count,
        } = &requested
        else {
            unreachable!("auto split resolves to an explicit split")
        };
        ColumnTransformation::split_with_blank_headers(
            *source_column,
            separator.clone(),
            *output_count,
            auto_source_header,
        )?
    } else {
        resolve_column_transformation(&requested, output_headers, session.dialect.has_header)?
    };
    let source_size_before = session.file_size;
    let job = session.start_save_as_with_transformation(
        BTreeMap::new(),
        BTreeMap::new(),
        transformation.clone(),
        &destination,
    )?;
    let (outcome, progress, cancellation) =
        wait_for_save_as(job, cancel_after_bytes, Duration::from_millis(1))?;
    let source_size_after = std::fs::metadata(session.path())?.len();
    if source_size_after != source_size_before {
        return Err("source file size changed during Save As".into());
    }
    let save_peak_rss = peak_rss_bytes();
    let save_current_rss = current_rss_bytes();

    let (outcome_label, published_bytes, validated_rows, validation_elapsed) = match &outcome {
        SaveAsOutcome::Complete(summary) => {
            let output_size = std::fs::metadata(&summary.destination)?.len();
            if summary.bytes_written != progress.bytes_written
                || output_size != summary.bytes_written
            {
                return Err("published output does not match Save As progress".into());
            }
            let (sample_rows, total_data_rows, validation_elapsed) =
                validate_saved_transformation(&session, &summary.destination, &transformation)?;
            (
                "complete",
                Some(output_size),
                Some((sample_rows, total_data_rows)),
                Some(validation_elapsed),
            )
        }
        SaveAsOutcome::Cancelled => {
            if destination.exists() {
                return Err("cancelled Save As published a destination file".into());
            }
            ("cancelled", None, None, None)
        }
    };

    println!("Quarry structural transformation Save As benchmark\n");
    println!("Source: {}", session.path().display());
    println!(
        "Source size: {} ({} bytes)",
        human_bytes(source_size_before),
        source_size_before
    );
    println!("Destination: {}", destination.display());
    println!(
        "Build: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("Save As cache state: {cache_state}");
    if let Some((analysis, rows_scanned, output_count)) = split_analysis {
        println!("Split width: full-file analysis");
        println!(
            "Split analysis bytes scanned: {} ({} bytes)",
            human_bytes(analysis.bytes_scanned),
            analysis.bytes_scanned
        );
        println!("Split analysis rows scanned: {rows_scanned}");
        println!("Discovered output columns: {output_count}");
        println!(
            "Split analysis time: {:.3} s",
            analysis.elapsed.as_secs_f64()
        );
        println!(
            "Split analysis throughput: {}/s",
            human_bytes(rate(analysis.bytes_scanned, analysis.elapsed))
        );
    }
    match &requested {
        RequestedColumnTransformation::Split {
            source_column,
            separator,
            output_count,
        } => {
            println!("Transformation: split");
            println!("Source column: {}", source_column + 1);
            println!("Separator: {}", render_field(separator));
            println!("Output columns: {output_count}");
        }
        RequestedColumnTransformation::Join {
            source_columns,
            separator,
        } => {
            println!("Transformation: join");
            println!(
                "Source columns: {}",
                source_columns
                    .iter()
                    .map(|column| (column + 1).to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            println!("Separator: {}", render_field(separator));
        }
        RequestedColumnTransformation::SplitAuto { .. } => {
            unreachable!("auto split resolves before reporting")
        }
    }
    println!("Outcome: {outcome_label}");
    println!(
        "Bytes scanned: {} ({} bytes) of {} ({} bytes)",
        human_bytes(progress.bytes_scanned),
        progress.bytes_scanned,
        human_bytes(progress.total_bytes),
        progress.total_bytes
    );
    println!(
        "Output bytes written: {} ({} bytes)",
        human_bytes(progress.bytes_written),
        progress.bytes_written
    );
    println!("Save As time: {:.3} s", progress.elapsed.as_secs_f64());
    println!(
        "Scan throughput: {}/s",
        human_bytes(rate(progress.bytes_scanned, progress.elapsed))
    );
    println!(
        "Output throughput: {}/s",
        human_bytes(rate(progress.bytes_written, progress.elapsed))
    );
    println!("Source size unchanged: yes ({source_size_after} bytes)");
    println!(
        "Destination published: {}",
        if published_bytes.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    if let Some(bytes) = published_bytes {
        println!("Published size: {} ({bytes} bytes)", human_bytes(bytes));
    }
    if let (Some((samples, total_rows)), Some(elapsed)) = (validated_rows, validation_elapsed) {
        println!(
            "Validated transformed sample rows: {samples} (first, middle, and final of {total_rows} data rows)"
        );
        if session.dialect.has_header {
            println!("Validated transformed header and output column count: yes");
        } else if total_rows > 0 {
            println!("Validated sampled output column counts: yes");
        }
        println!(
            "Validation indexes and reads: {:.3} s",
            elapsed.as_secs_f64()
        );
    }
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
        "Current process memory after Save As: {}",
        optional_bytes(save_current_rss)
    );
    println!(
        "Peak process memory through Save As: {}",
        optional_bytes(save_peak_rss)
    );
    Ok(())
}

fn parse_join_columns(value: &str) -> CliResult<Vec<usize>> {
    let mut columns = Vec::new();
    let mut seen = BTreeSet::new();
    for value in value.split(',') {
        let column = value.parse::<usize>()?;
        if column == 0 {
            return Err("join columns must be at least 1".into());
        }
        if column > MAX_TRANSFORMATION_COLUMNS || columns.len() == MAX_TRANSFORMATION_COLUMNS {
            return Err("join columns exceed the supported limit".into());
        }
        let column = column - 1;
        if !seen.insert(column) {
            return Err("join columns must be unique".into());
        }
        columns.push(column);
    }
    if columns.len() < 2 {
        return Err("join requires at least two columns".into());
    }
    Ok(columns)
}

fn resolve_column_transformation(
    requested: &RequestedColumnTransformation,
    output_headers: Vec<Vec<u8>>,
    has_header: bool,
) -> CliResult<ColumnTransformation> {
    if !has_header && !output_headers.is_empty() {
        return Err("output headers require a source header row".into());
    }
    match requested {
        RequestedColumnTransformation::Split {
            source_column,
            separator,
            output_count,
        } => {
            if has_header && output_headers.len() != *output_count {
                return Err(format!(
                    "split requires exactly {output_count} --output-header values"
                )
                .into());
            }
            Ok(ColumnTransformation::Split {
                source_column: *source_column,
                separator: separator.clone(),
                output_count: *output_count,
                output_headers: has_header.then_some(output_headers),
            })
        }
        RequestedColumnTransformation::Join {
            source_columns,
            separator,
        } => {
            if has_header && output_headers.len() != 1 {
                return Err("join requires exactly one --output-header value".into());
            }
            Ok(ColumnTransformation::Join {
                source_columns: source_columns.clone(),
                separator: separator.clone(),
                output_header: has_header.then(|| output_headers.into_iter().next().unwrap()),
            })
        }
        RequestedColumnTransformation::SplitAuto { .. } => {
            unreachable!("auto split resolves before transformation")
        }
    }
}

fn wait_for_split_analysis(
    job: SplitAnalysisJob,
) -> CliResult<(SplitAnalysisOutcome, SplitAnalysisProgress)> {
    while !job.progress().done {
        thread::sleep(Duration::from_millis(1));
    }
    let progress = job.progress();
    Ok((job.wait()?, progress))
}

fn validate_saved_transformation(
    source: &Session,
    destination: &Path,
    transformation: &ColumnTransformation,
) -> CliResult<(usize, u64, Duration)> {
    let started = Instant::now();
    let output = Session::open(
        destination,
        OpenOptions {
            rows: 1,
            delimiter: Some(source.dialect.delimiter),
            header_mode: if source.dialect.has_header {
                HeaderMode::FirstRow
            } else {
                HeaderMode::NoHeader
            },
            ..OpenOptions::default()
        },
    )?;
    let source_index = source.start_indexing(IndexConfig::default())?.wait()?;
    let output_index = output.start_indexing(IndexConfig::default())?.wait()?;
    if output_index.indexed_rows() != source_index.indexed_rows() {
        return Err("transformed output record count changed".into());
    }

    if source.dialect.has_header {
        let source_header = source
            .first_rows
            .first()
            .ok_or("source sample is missing its header")?;
        let output_header = output
            .first_rows
            .first()
            .ok_or("transformed output is missing its header")?;
        let expected_header = transformation.transform_header_fields(&source_header.fields)?;
        if output_header.fields != expected_header {
            return Err("transformed output header failed read-back validation".into());
        }
    }

    let data_start = u64::from(source.dialect.has_header);
    let total_data_rows = source_index.indexed_rows().saturating_sub(data_start);
    let mut sample_rows = Vec::new();
    if total_data_rows > 0 {
        sample_rows.extend([
            data_start,
            data_start + (total_data_rows - 1) / 2,
            data_start + total_data_rows - 1,
        ]);
        sample_rows.sort_unstable();
        sample_rows.dedup();
    }
    for &record_row in &sample_rows {
        let source_row = source
            .read_rows(&source_index, record_row, 1)?
            .into_iter()
            .next()
            .ok_or("source validation row is missing")?;
        let output_row = output
            .read_rows(&output_index, record_row, 1)?
            .into_iter()
            .next()
            .ok_or("transformed output validation row is missing")?;
        if output_row.fields != transformation.transform_fields(&source_row.fields)? {
            return Err(format!(
                "transformed output data row {} failed read-back validation",
                physical_to_data_row(record_row, source.dialect.has_header)
            )
            .into());
        }
    }
    Ok((sample_rows.len(), total_data_rows, started.elapsed()))
}

fn wait_for_save_as(
    job: SaveAsJob,
    cancel_after_bytes: Option<u64>,
    poll_interval: Duration,
) -> CliResult<SaveAsRun> {
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
        return Err("Save As finished before cancellation threshold".into());
    }
    if cancel_after_bytes.is_some() && !matches!(&outcome, SaveAsOutcome::Cancelled) {
        return Err("Save As completed before cancellation took effect".into());
    }
    if cancel_after_bytes.is_some() && progress.bytes_scanned >= progress.total_bytes {
        return Err("Save As reached end of file before cancellation took effect".into());
    }
    Ok((outcome, progress, cancellation))
}

fn wait_for_replace_all(
    job: ReplaceAllJob,
    cancel_after_bytes: Option<u64>,
    poll_interval: Duration,
) -> CliResult<ReplaceAllRun> {
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
        return Err("Replace All finished before cancellation threshold".into());
    }
    if cancel_after_bytes.is_some() && !matches!(&outcome, ReplaceAllOutcome::Cancelled) {
        return Err("Replace All completed before cancellation took effect".into());
    }
    if cancel_after_bytes.is_some() && progress.bytes_scanned >= progress.total_bytes {
        return Err("Replace All reached end of file before cancellation took effect".into());
    }
    Ok((outcome, progress, cancellation))
}

fn validate_saved_edits(
    destination: &Path,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    dialect: Dialect,
) -> CliResult<Duration> {
    let started = Instant::now();
    let session = Session::open(
        destination,
        OpenOptions {
            delimiter: Some(dialect.delimiter),
            header_mode: if dialect.has_header {
                HeaderMode::FirstRow
            } else {
                HeaderMode::NoHeader
            },
            ..OpenOptions::default()
        },
    )?;
    let index = session.start_indexing(IndexConfig::default())?.wait()?;
    let mut current_row = None;
    let mut row = None;
    for (&(record_row, column), expected) in cell_edits {
        if current_row != Some(record_row) {
            row = session.read_rows(&index, record_row, 1)?.into_iter().next();
            current_row = Some(record_row);
        }
        let actual = row
            .as_ref()
            .and_then(|row| row.fields.get(column))
            .ok_or("saved edit position is out of range")?;
        if actual != expected {
            return Err(format!(
                "saved data row {}, column {} does not contain the requested value",
                physical_to_data_row(record_row, session.dialect.has_header),
                column + 1
            )
            .into());
        }
    }
    Ok(started.elapsed())
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
    let mut sampled_until = 0_u64;
    for start in starts {
        let start = start.max(sampled_until);
        if start >= matches {
            continue;
        }
        let end = start.saturating_add(FILTER_SAMPLE_ROWS as u64).min(matches);
        let count = usize::try_from(end - start)?;
        for found in session.read_filtered_rows(index, start, count)? {
            record_filter_sample(&mut samples, &found);
        }
        sampled_until = end;
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
    let mut metrics_only = false;
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
            "--metrics-only" => metrics_only = true,
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
    if !metrics_only {
        for (number, row) in session.first_rows.iter().take(5).enumerate() {
            let rendered = row
                .fields
                .iter()
                .map(|field| render_field(field))
                .collect::<Vec<_>>()
                .join(" | ");
            println!("row {number} @{}: {rendered}", row.offset);
        }
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
                print_jump(
                    &session,
                    &index,
                    start,
                    jump_count,
                    Some(progress),
                    !metrics_only,
                )?;
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
        print_jump(&session, &index, start, jump_count, None, !metrics_only)?;
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
    render_rows: bool,
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
    if render_rows {
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
        FilterSamples, RAW_HEADER_COMPARE_BUFFER_BYTES, compare_raw_headers, data_search_position,
        edit_save_as_command, export_command, filter_command, fnv1a64_file, generate_file,
        latency_stats, parse_header_mode, parse_size, parse_sort_direction, physical_to_data_row,
        record_filter_sample, replace_all_save_as_command, sample_filtered_rows, search_command,
        sort_artifact_permissions, sort_save_as_command, transform_save_as_command,
        validate_saved_transformation, validate_sort_completion_evidence, viewport_command,
        wait_for_save_as,
    };
    use quarry_core::{
        ColumnTransformation, FilterOperator, FilterQuery, HeaderMode, IndexConfig, OpenOptions,
        Session, SortDirection,
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
                vec!["missing.csv", "--column", "1"],
                "filter requires --operator",
            ),
            (
                vec!["missing.csv", "--column", "1", "--operator", "equals"],
                "filter requires --value",
            ),
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
                "--operator must be contains, equals, or not-equals",
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
            (
                vec![
                    "missing.csv",
                    "--column",
                    "1",
                    "--operator",
                    "equals",
                    "--value",
                    "x",
                    "--and",
                    "2",
                    "equals",
                ],
                "--and requires COLUMN contains|equals|not-equals VALUE",
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
                    "--and",
                    "0",
                    "equals",
                    "x",
                ],
                "AND filter column must be at least 1",
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
                    "--and",
                    "2",
                    "contains",
                    "",
                ],
                "AND contains filter value must not be empty",
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
                    "--and",
                    "2",
                    "regex",
                    "x",
                ],
                "--and operator must be contains, equals, or not-equals",
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
            ("not-equals", "needle"),
            ("equals", ""),
            ("not-equals", ""),
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

        let session = Session::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let index = session
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"needle".to_vec(),
            ))
            .unwrap()
            .wait()
            .unwrap();
        let samples = sample_filtered_rows(&session, &index).unwrap();
        let mut expected = FilterSamples::default();
        for found in session.read_filtered_rows(&index, 0, 2).unwrap() {
            record_filter_sample(&mut expected, &found);
        }
        assert_eq!(index.matches_found(), 2);
        assert_eq!(samples.rows_read, 2);
        assert_eq!(samples.checksum, expected.checksum);

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
    fn filter_command_accepts_repeatable_and_predicates() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "quarry-filter-and-command-{}-{suffix}.csv",
            std::process::id()
        ));
        fs::write(
            &path,
            b"name,state,status,kind\none,TX,active,gold\ntwo,TX,active,silver\nthree,TX,inactive,gold\nfour,CA,active,gold\nfive,TX,active,gold\n",
        )
        .unwrap();

        let mut and_args = vec![
            path.to_string_lossy().into_owned(),
            "--column".into(),
            "2".into(),
            "--operator".into(),
            "equals".into(),
            "--value".into(),
            "TX".into(),
        ];
        and_args.extend([
            "--and".into(),
            "3".into(),
            "equals".into(),
            "active".into(),
            "--and".into(),
            "4".into(),
            "equals".into(),
            "gold".into(),
        ]);
        filter_command(and_args).unwrap();

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
    fn export_command_preserves_raw_records_and_source() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-export-command-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let destination = directory.join("filtered.csv");
        let source_bytes = b"name,note,state\r\none,\"line, \"\"one\"\"\r\nline two\",TX\r\ntwo,plain,CA\r\nthree,\"other\",TX\r\n";
        let expected = b"name,note,state\r\none,\"line, \"\"one\"\"\r\nline two\",TX\r\nthree,\"other\",TX\r\n";
        fs::write(&source, source_bytes).unwrap();

        export_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            destination.to_string_lossy().into_owned(),
            "--column".into(),
            "3".into(),
            "--operator".into(),
            "equals".into(),
            "--value".into(),
            "TX".into(),
        ])
        .unwrap();

        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn export_command_cancellation_leaves_no_output_or_temporary_file() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-export-cancellation-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let destination = directory.join("filtered.csv");
        generate_file(&source, 64 * 1024 * 1024, 11, b',', 7).unwrap();

        export_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            destination.to_string_lossy().into_owned(),
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

        assert!(!destination.exists());
        let names = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], source.file_name().unwrap());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn edit_save_as_command_validates_sparse_cells_and_preserves_source() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-edit-save-as-command-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let destination = directory.join("edited.csv");
        let source_bytes = b"name,note,state\r\none,\"line, \"\"one\"\"\r\nline two\",TX\r\ntwo,plain,CA\r\nthree,other,WA";
        let expected = b"name,note,state\r\none,\"changed, \"\"quoted\"\"\nnext\",TX\r\ntwo,plain,CA\r\nthree,other,TX";
        fs::write(&source, source_bytes).unwrap();

        edit_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            destination.to_string_lossy().into_owned(),
            "--edit".into(),
            "1".into(),
            "2".into(),
            "changed, \"quoted\"\nnext".into(),
            "--edit".into(),
            "3".into(),
            "3".into(),
            "TX".into(),
        ])
        .unwrap();

        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn edit_save_as_command_reuses_the_resolved_source_dialect_for_validation() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-edit-save-as-dialect-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.tsv");
        let destination = directory.join("edited.tsv");
        fs::write(&source, b"1\t2\n3\t4\n").unwrap();

        edit_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            destination.to_string_lossy().into_owned(),
            "--edit".into(),
            "1".into(),
            "2".into(),
            "a,b,c".into(),
            "--edit".into(),
            "2".into(),
            "2".into(),
            "d,e,f".into(),
        ])
        .unwrap();

        assert_eq!(fs::read(&source).unwrap(), b"1\t2\n3\t4\n");
        assert_eq!(fs::read(&destination).unwrap(), b"1\ta,b,c\n3\td,e,f\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn edit_save_as_command_rejects_invalid_and_duplicate_edits() {
        let error = edit_save_as_command(Vec::new()).unwrap_err();
        assert_eq!(error.to_string(), "edit-save-as requires a file path");

        let error = edit_save_as_command(vec!["missing.csv".into()]).unwrap_err();
        assert_eq!(error.to_string(), "edit-save-as requires --output");

        let error = edit_save_as_command(vec![
            "missing.csv".into(),
            "--output".into(),
            "output.csv".into(),
        ])
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "edit-save-as requires at least one --edit"
        );

        for (row, column) in [("0", "1"), ("1", "0")] {
            let error = edit_save_as_command(vec![
                "missing.csv".into(),
                "--output".into(),
                "output.csv".into(),
                "--edit".into(),
                row.into(),
                column.into(),
                "value".into(),
            ])
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "edit data row and column must be at least 1"
            );
        }

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-edit-save-as-duplicate-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let destination = directory.join("edited.csv");
        fs::write(&source, b"name,value\none,1\n").unwrap();
        let error = edit_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            destination.to_string_lossy().into_owned(),
            "--edit".into(),
            "1".into(),
            "2".into(),
            "first".into(),
            "--edit".into(),
            "1".into(),
            "2".into(),
            "second".into(),
        ])
        .unwrap_err();
        assert_eq!(error.to_string(), "duplicate edit for data row 1, column 2");
        assert!(!destination.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn edit_save_as_command_cancellation_preserves_source_and_cleans_output() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-edit-save-as-cancellation-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let destination = directory.join("edited.csv");
        generate_file(&source, 64 * 1024 * 1024, 11, b',', 7).unwrap();
        let source_size = fs::metadata(&source).unwrap().len();

        edit_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            destination.to_string_lossy().into_owned(),
            "--edit".into(),
            "1".into(),
            "1".into(),
            "cancelled".into(),
            "--cancel-after-bytes".into(),
            "1".into(),
        ])
        .unwrap();

        assert_eq!(fs::metadata(&source).unwrap().len(), source_size);
        assert!(!destination.exists());
        let names = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], source.file_name().unwrap());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn replace_all_save_as_is_exact_and_cancellation_cleans_private_output() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-replace-all-command-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let destination = directory.join("replaced.csv");
        let source_bytes = b"name,note\none,alpha alpha\ntwo,beta\n";
        fs::write(&source, source_bytes).unwrap();

        replace_all_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            destination.to_string_lossy().into_owned(),
            "--query".into(),
            "alpha".into(),
            "--replacement".into(),
            "gamma".into(),
        ])
        .unwrap();

        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"name,note\none,gamma gamma\ntwo,beta\n"
        );

        let cancellation_directory = directory.join("cancel");
        fs::create_dir(&cancellation_directory).unwrap();
        let cancellation_source = cancellation_directory.join("source.csv");
        let cancellation_destination = cancellation_directory.join("replaced.csv");
        generate_file(&cancellation_source, 64 * 1024 * 1024, 11, b',', 7).unwrap();
        let source_size = fs::metadata(&cancellation_source).unwrap().len();
        replace_all_save_as_command(vec![
            cancellation_source.to_string_lossy().into_owned(),
            "--output".into(),
            cancellation_destination.to_string_lossy().into_owned(),
            "--query".into(),
            "a".into(),
            "--replacement".into(),
            "A".into(),
            "--cancel-after-bytes".into(),
            "1".into(),
        ])
        .unwrap();

        assert_eq!(
            fs::metadata(&cancellation_source).unwrap().len(),
            source_size
        );
        assert!(!cancellation_destination.exists());
        let names = fs::read_dir(&cancellation_directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![cancellation_source.file_name().unwrap()]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sort_save_as_parses_required_arguments_and_dialect_overrides() {
        assert!(matches!(
            parse_sort_direction("asc").unwrap(),
            SortDirection::Ascending
        ));
        assert!(matches!(
            parse_sort_direction("desc").unwrap(),
            SortDirection::Descending
        ));
        assert_eq!(
            parse_sort_direction("ascending").unwrap_err().to_string(),
            "--order must be asc or desc"
        );
        assert_eq!(parse_header_mode("auto").unwrap(), HeaderMode::Auto);
        assert_eq!(
            parse_header_mode("first-row").unwrap(),
            HeaderMode::FirstRow
        );
        assert_eq!(parse_header_mode("none").unwrap(), HeaderMode::NoHeader);
        assert_eq!(
            parse_header_mode("yes").unwrap_err().to_string(),
            "--header must be auto, first-row, or none"
        );

        for (args, expected) in [
            (vec![], "sort-save-as requires a source file path"),
            (
                vec!["source.csv"],
                "sort-save-as requires a destination file path",
            ),
            (
                vec!["source.csv", "sorted.csv"],
                "sort-save-as requires --column",
            ),
            (
                vec!["source.csv", "sorted.csv", "--column", "1"],
                "sort-save-as requires --order",
            ),
            (
                vec![
                    "source.csv",
                    "sorted.csv",
                    "--column",
                    "0",
                    "--order",
                    "asc",
                ],
                "sort column must be at least 1",
            ),
            (
                vec![
                    "source.csv",
                    "sorted.csv",
                    "--column",
                    "1",
                    "--order",
                    "asc",
                    "--cancel-after-bytes",
                    "0",
                ],
                "cancel-after-bytes must be non-zero",
            ),
        ] {
            let error =
                sort_save_as_command(args.into_iter().map(str::to_owned).collect()).unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn sort_file_hash_is_deterministic() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "quarry-sort-hash-{}-{suffix}.txt",
            std::process::id()
        ));
        fs::write(&path, b"").unwrap();
        assert_eq!(fnv1a64_file(&path).unwrap(), 0xcbf2_9ce4_8422_2325);
        fs::write(&path, b"a").unwrap();
        assert_eq!(fnv1a64_file(&path).unwrap(), 0xaf63_dc4c_8601_ec8c);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sort_completion_requires_multiset_and_stable_tie_evidence() {
        validate_sort_completion_evidence(true, true).unwrap();
        assert_eq!(
            validate_sort_completion_evidence(false, true)
                .unwrap_err()
                .to_string(),
            "sort did not verify record multiset preservation"
        );
        assert_eq!(
            validate_sort_completion_evidence(true, false)
                .unwrap_err()
                .to_string(),
            "sort did not verify stable equal-key ordering"
        );
    }

    #[test]
    fn sort_artifact_permissions_report_observed_mode_or_not_published() {
        assert_eq!(
            sort_artifact_permissions(None).unwrap(),
            "n/a (not published)"
        );

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "quarry-sort-permissions-{}-{suffix}.tmp",
            std::process::id()
        ));
        fs::write(&path, b"artifact").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o640);
            fs::set_permissions(&path, permissions).unwrap();
            assert_eq!(
                sort_artifact_permissions(Some(&path)).unwrap(),
                "0640 (observed Unix mode)"
            );
        }
        #[cfg(not(unix))]
        assert_eq!(
            sort_artifact_permissions(Some(&path)).unwrap(),
            "n/a (Unix mode unavailable)"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn raw_header_comparison_streams_a_single_record_header() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-sort-raw-header-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        let header = vec![b'a'; RAW_HEADER_COMPARE_BUFFER_BYTES * 2 + 17];
        fs::write(&source, &header).unwrap();
        fs::write(&destination, &header).unwrap();

        let open = |path| {
            Session::open(
                path,
                OpenOptions {
                    rows: 1,
                    delimiter: Some(b','),
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
            )
            .unwrap()
        };
        let source_session = open(&source);
        let source_index = source_session
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        let destination_session = open(&destination);
        let destination_index = destination_session
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(
            compare_raw_headers(
                &source,
                &source_session,
                &source_index,
                &destination,
                &destination_session,
                &destination_index,
            )
            .unwrap(),
            Some(header.len() as u64)
        );

        let mut changed = header;
        *changed.last_mut().unwrap() = b'b';
        fs::write(&destination, changed).unwrap();
        assert_eq!(
            compare_raw_headers(
                &source,
                &source_session,
                &source_index,
                &destination,
                &destination_session,
                &destination_index,
            )
            .unwrap_err()
            .to_string(),
            "sorted output raw header changed"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sort_save_as_writes_exact_stable_output_and_preserves_source() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-sort-save-as-command-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.psv");
        let destination = directory.join("sorted.psv");
        let source_bytes = b"name|note\r\nbeta|\"line one\nline two\"\r\nalpha|\"comma, value\"\r\nalpha|plain\r\n";
        let expected = b"name|note\r\nalpha|\"comma, value\"\r\nalpha|plain\r\nbeta|\"line one\nline two\"\r\n";
        fs::write(&source, source_bytes).unwrap();

        sort_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
            "--column".into(),
            "1".into(),
            "--order".into(),
            "asc".into(),
            "--delimiter".into(),
            "|".into(),
            "--header".into(),
            "first-row".into(),
        ])
        .unwrap();

        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sort_save_as_cancellation_preserves_source_and_cleans_output() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-sort-save-as-cancellation-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        generate_file(&source, 64 * 1024 * 1024, 11, b',', 7).unwrap();
        let source_size = fs::metadata(&source).unwrap().len();

        sort_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
            "--column".into(),
            "1".into(),
            "--order".into(),
            "desc".into(),
            "--header".into(),
            "first-row".into(),
            "--cancel-after-bytes".into(),
            "1".into(),
        ])
        .unwrap();

        assert_eq!(fs::metadata(&source).unwrap().len(), source_size);
        assert!(!destination.exists());
        let names = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![source.file_name().unwrap()]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transform_save_as_command_validates_split_and_join_outputs() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-transform-save-as-command-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let split = directory.join("split.csv");
        let auto_split = directory.join("auto-split.csv");
        let joined = directory.join("joined.csv");
        let source_bytes = b"name,city,state\nAda::Lovelace,London,UK\nGrace,Arlington,US\n";
        fs::write(&source, source_bytes).unwrap();

        transform_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            auto_split.to_string_lossy().into_owned(),
            "--split-auto".into(),
            "1".into(),
            "::".into(),
        ])
        .unwrap();

        transform_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            split.to_string_lossy().into_owned(),
            "--split".into(),
            "1".into(),
            "::".into(),
            "2".into(),
            "--output-header".into(),
            "first".into(),
            "--output-header".into(),
            "last".into(),
        ])
        .unwrap();

        transform_save_as_command(vec![
            source.to_string_lossy().into_owned(),
            "--output".into(),
            joined.to_string_lossy().into_owned(),
            "--join".into(),
            "2,3".into(),
            ", ".into(),
            "--output-header".into(),
            "location".into(),
        ])
        .unwrap();

        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(
            fs::read(&split).unwrap(),
            b"first,last,city,state\nAda,Lovelace,London,UK\nGrace,,Arlington,US\n"
        );
        assert_eq!(
            fs::read(&auto_split).unwrap(),
            b"name,,city,state\nAda,Lovelace,London,UK\nGrace,,Arlington,US\n"
        );
        assert_eq!(
            fs::read(&joined).unwrap(),
            b"name,location\nAda::Lovelace,\"London, UK\"\nGrace,\"Arlington, US\"\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transform_validation_handles_headered_and_headerless_bom() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "quarry-transform-save-as-bom-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let destination = directory.join("split.csv");
        fs::write(
            &source,
            b"\xEF\xBB\xBFname,city\nAda::Lovelace,London\nGrace,Arlington\n",
        )
        .unwrap();
        let session = Session::open(
            &source,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let transformation = ColumnTransformation::Split {
            source_column: 0,
            separator: b"::".to_vec(),
            output_count: 2,
            output_headers: Some(vec![b"first".to_vec(), b"last".to_vec()]),
        };
        let job = session
            .start_save_as_with_transformation(
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
                transformation.clone(),
                &destination,
            )
            .unwrap();
        wait_for_save_as(job, None, Duration::from_millis(1)).unwrap();

        let (samples, total_rows, _) =
            validate_saved_transformation(&session, &destination, &transformation).unwrap();
        assert_eq!((samples, total_rows), (2, 2));
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"\xEF\xBB\xBFfirst,last,city\nAda,Lovelace,London\nGrace,,Arlington\n"
        );

        let headerless_source = directory.join("headerless.csv");
        let headerless_destination = directory.join("joined.csv");
        fs::write(&headerless_source, b"\xEF\xBB\xBFone,two\nthree,four\n").unwrap();
        let headerless_session = Session::open(
            &headerless_source,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let join = ColumnTransformation::Join {
            source_columns: vec![1, 0],
            separator: b",\n".to_vec(),
            output_header: None,
        };
        let job = headerless_session
            .start_save_as_with_transformation(
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
                join.clone(),
                &headerless_destination,
            )
            .unwrap();
        wait_for_save_as(job, None, Duration::from_millis(1)).unwrap();
        validate_saved_transformation(&headerless_session, &headerless_destination, &join).unwrap();
        assert_eq!(
            fs::read(&headerless_destination).unwrap(),
            b"\xEF\xBB\xBF\"two,\none\"\n\"four,\nthree\"\n"
        );

        let quoted_bom_source = directory.join("quoted-bom.csv");
        let quoted_bom_destination = directory.join("quoted-bom-joined.csv");
        fs::write(&quoted_bom_source, b"\"\xEF\xBB\xBFone\",two\n").unwrap();
        let quoted_bom_session = Session::open(
            &quoted_bom_source,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let quoted_bom_join = ColumnTransformation::Join {
            source_columns: vec![1, 0],
            separator: b"|".to_vec(),
            output_header: None,
        };
        let job = quoted_bom_session
            .start_save_as_with_transformation(
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
                quoted_bom_join.clone(),
                &quoted_bom_destination,
            )
            .unwrap();
        wait_for_save_as(job, None, Duration::from_millis(1)).unwrap();
        validate_saved_transformation(
            &quoted_bom_session,
            &quoted_bom_destination,
            &quoted_bom_join,
        )
        .unwrap();
        assert_eq!(
            fs::read(&quoted_bom_destination).unwrap(),
            b"two|\xEF\xBB\xBFone\n"
        );

        let leading_bom_source = directory.join("leading-bom.csv");
        let leading_bom_destination = directory.join("leading-bom-joined.csv");
        fs::write(&leading_bom_source, b"\"\xEF\xBB\xBFone\",two\n").unwrap();
        transform_save_as_command(vec![
            leading_bom_source.to_string_lossy().into_owned(),
            "--output".into(),
            leading_bom_destination.to_string_lossy().into_owned(),
            "--join".into(),
            "1,2".into(),
            "|".into(),
        ])
        .unwrap();
        assert_eq!(
            fs::read(&leading_bom_destination).unwrap(),
            b"\"\xEF\xBB\xBFone|two\"\n"
        );

        let empty_source = directory.join("empty-fields.csv");
        let empty_destination = directory.join("empty-fields-joined.csv");
        fs::write(&empty_source, b",").unwrap();
        transform_save_as_command(vec![
            empty_source.to_string_lossy().into_owned(),
            "--output".into(),
            empty_destination.to_string_lossy().into_owned(),
            "--join".into(),
            "1,2".into(),
            String::new(),
        ])
        .unwrap();
        assert_eq!(fs::read(&empty_destination).unwrap(), b"\"\"");
        fs::remove_dir_all(directory).unwrap();
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
