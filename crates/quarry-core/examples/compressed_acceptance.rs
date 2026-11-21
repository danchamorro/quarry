//! Release-mode acceptance probe for gzip-compressed delimited files.
//! SOURCE and TEMP_DIR are positional. Optional flags set the decompressed size
//! limit, cancel decompression after a number of written bytes, and run a filtered
//! export and an uncompressed Save Copy As from the verified private copy.
use quarry_core::{
    CompressedBootstrap, CompressedOpenConfig, CompressedOpenOutcome, FilterExportOutcome,
    FilterOperator, FilterQuery, OpenOptions, QuarryError, Row, SaveAsOutcome, WorkingCopyCleanup,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

type ProbeResult = Result<(), Box<dyn std::error::Error>>;

struct Options {
    source: PathBuf,
    temp: PathBuf,
    max_decompressed_bytes: Option<u64>,
    cancel_after_bytes: Option<u64>,
    filter_column: usize,
    filter_value: Option<String>,
    export: Option<PathBuf>,
    save_copy: Option<PathBuf>,
}

fn options() -> Result<Options, Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() < 2 {
        return Err("usage: compressed_acceptance SOURCE TEMP_DIR [flags]".into());
    }
    let mut options = Options {
        source: PathBuf::from(&args[0]),
        temp: PathBuf::from(&args[1]),
        max_decompressed_bytes: None,
        cancel_after_bytes: None,
        filter_column: 0,
        filter_value: None,
        export: None,
        save_copy: None,
    };
    let mut index = 2;
    while index < args.len() {
        let flag = args[index].to_str().ok_or("non-text benchmark flag")?;
        let value = args.get(index + 1).ok_or("missing flag value")?;
        let text = value.to_str().ok_or("non-text flag value")?;
        match flag {
            "--max-decompressed-bytes" => options.max_decompressed_bytes = Some(text.parse()?),
            "--cancel-after-bytes" => options.cancel_after_bytes = Some(text.parse()?),
            "--filter-column" => options.filter_column = text.parse()?,
            "--filter-value" => options.filter_value = Some(text.to_owned()),
            "--export" => options.export = Some(PathBuf::from(value)),
            "--save-copy" => options.save_copy = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown flag {flag}").into()),
        }
        index += 2;
    }
    Ok(options)
}

fn error_kind(error: &QuarryError) -> &'static str {
    match error {
        QuarryError::CompressedFile { .. } => "CompressedFile",
        QuarryError::DecompressionLimitExceeded { .. } => "DecompressionLimitExceeded",
        QuarryError::CompressedReadOnly => "CompressedReadOnly",
        QuarryError::SourceChanged => "SourceChanged",
        QuarryError::InvalidOption(_) => "InvalidOption",
        _ => "Other",
    }
}

/// Keeps every reported value on one `key=value` line.
fn single_line(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn cell(row: &Row, column: usize) -> String {
    row.fields
        .get(column)
        .map(|field| single_line(&String::from_utf8_lossy(field)))
        .unwrap_or_default()
}

fn report_error(stage: &str, error: &QuarryError) {
    println!("result=error");
    println!("error_stage={stage}");
    println!("error_kind={}", error_kind(error));
    println!("error_message={}", single_line(&error.to_string()));
}

/// Counts private decompression workspaces left in the temporary folder.
fn finish_cleanup(temp: &Path, cleanup: &WorkingCopyCleanup) -> ProbeResult {
    let residual = std::fs::read_dir(temp)?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".quarry-compressed-")
        })
        .count();
    println!("residual_workspaces={residual}");
    let errors = cleanup.take_errors();
    println!("cleanup_errors={}", errors.len());
    for error in errors {
        eprintln!("cleanup_error={error}");
    }
    Ok(())
}

fn main() -> ProbeResult {
    let options = options()?;
    let cleanup = WorkingCopyCleanup::default();
    let started = Instant::now();
    let bootstrap = match CompressedBootstrap::open(
        &options.source,
        OpenOptions::default(),
        None,
        options.max_decompressed_bytes,
    ) {
        Ok(bootstrap) => bootstrap,
        Err(error) => {
            report_error("bootstrap", &error);
            return finish_cleanup(&options.temp, &cleanup);
        }
    };
    println!(
        "bootstrap_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    println!("bootstrap_first_rows={}", bootstrap.first_rows.len());
    println!(
        "bootstrap_compressed_bytes_read={}",
        bootstrap.compressed_bytes_read()
    );
    println!(
        "bootstrap_decompressed_bytes={}",
        bootstrap.metrics.bootstrap_bytes_read
    );
    println!("compressed_bytes={}", bootstrap.compressed_size());
    println!("format={}", bootstrap.format().label());
    println!("has_header={}", bootstrap.dialect.has_header);
    for (label, position) in [("first", 0), ("second", 1)] {
        if let Some(row) = bootstrap.first_rows.get(position) {
            println!("bootstrap_{label}_cell={}", cell(row, 0));
        }
    }

    let mut config = CompressedOpenConfig::new(&options.temp);
    config.max_decompressed_bytes = options.max_decompressed_bytes;
    let job = match bootstrap.start_decompression(config, cleanup.clone()) {
        Ok(job) => job,
        Err(error) => {
            report_error("decompress", &error);
            return finish_cleanup(&options.temp, &cleanup);
        }
    };
    let mut requested = None;
    let mut peak = 0;
    let mut live_viewport = false;
    let mut last_report = Instant::now();
    loop {
        let progress = job.progress();
        peak = progress.decompressed_bytes_written.max(peak);
        if progress.done {
            break;
        }
        // A progressive read of rows that are already decompressed while the job runs.
        if !live_viewport && progress.rows_scanned > 64 {
            live_viewport = true;
            let read = Instant::now();
            match job.read_rows(32, 1) {
                Ok(rows) if !rows.is_empty() => {
                    println!("live_viewport_cell={}", cell(&rows[0], 0));
                    println!("live_viewport_rows_indexed={}", progress.rows_scanned);
                    println!("live_viewport_seconds={:.9}", read.elapsed().as_secs_f64());
                }
                Ok(_) => println!("live_viewport_error=no rows"),
                Err(error) => {
                    println!("live_viewport_error={}", single_line(&error.to_string()))
                }
            }
        }
        if requested.is_none()
            && options
                .cancel_after_bytes
                .is_some_and(|threshold| progress.decompressed_bytes_written >= threshold)
        {
            job.cancel();
            requested = Some(Instant::now());
            println!(
                "cancel_requested_after_bytes={}",
                progress.decompressed_bytes_written
            );
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            eprintln!(
                "phase=decompressing decompressed={} rows={}",
                progress.decompressed_bytes_written, progress.rows_scanned
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    let progress = job.progress();
    let outcome = job.wait();
    // The worker removes its private copy before the job reports completion.
    if let Some(requested) = requested {
        println!(
            "cancel_latency_seconds={:.9}",
            requested.elapsed().as_secs_f64()
        );
    }
    println!("peak_decompressed_bytes_observed={peak}");
    println!("compressed_bytes_read={}", progress.compressed_bytes_read);
    println!(
        "decompress_index_seconds={:.9}",
        progress.elapsed.as_secs_f64()
    );
    let (session, index) = match outcome {
        Ok(CompressedOpenOutcome::Complete { session, index }) => (session, index),
        Ok(CompressedOpenOutcome::Cancelled) => {
            println!("result=cancelled");
            return finish_cleanup(&options.temp, &cleanup);
        }
        Err(error) => {
            report_error("decompress", &error);
            return finish_cleanup(&options.temp, &cleanup);
        }
    };
    if options.cancel_after_bytes.is_some() {
        return Err("decompression finished before the requested cancellation point".into());
    }
    println!("rows_scanned={}", index.indexed_rows());
    println!("columns_found={}", index.maximum_width());
    println!("decompressed_bytes={}", session.file_size);
    println!("progress_rows_scanned={}", progress.rows_scanned);
    println!("progress_columns_found={}", progress.columns_found);

    let last = index.indexed_rows().saturating_sub(1);
    let mut checks = 0;
    let mut slowest: f64 = 0.0;
    for (label, row) in [
        ("first", 0),
        ("second", 1),
        ("middle", last / 2),
        ("last", last),
    ] {
        let read = Instant::now();
        let rows = session.read_rows(&index, row, 1)?;
        slowest = slowest.max(read.elapsed().as_secs_f64());
        let found = rows.first().ok_or("viewport returned no row")?;
        println!("viewport_{label}_row={row}");
        println!("viewport_{label}_cell={}", cell(found, 0));
        println!("viewport_{label}_fields={}", found.fields.len());
        checks += 1;
    }
    println!("viewport_checks={checks}");
    println!("viewport_slowest_seconds={slowest:.9}");

    if let Some(destination) = &options.export {
        let value = options
            .filter_value
            .clone()
            .ok_or("--export needs --filter-value")?;
        let query = FilterQuery::single(
            options.filter_column,
            FilterOperator::Contains,
            value.into_bytes(),
        );
        let export = Instant::now();
        match session.start_filtered_export(query, destination)?.wait()? {
            FilterExportOutcome::Complete(summary) => {
                println!("export_seconds={:.9}", export.elapsed().as_secs_f64());
                println!("export_rows={}", summary.rows_written);
                println!("export_bytes={}", summary.bytes_written);
            }
            FilterExportOutcome::Cancelled => return Err("export unexpectedly cancelled".into()),
        }
    }
    if let Some(destination) = &options.save_copy {
        let save = Instant::now();
        match session
            .start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), destination)?
            .wait()?
        {
            SaveAsOutcome::Complete(summary) => {
                println!("save_copy_seconds={:.9}", save.elapsed().as_secs_f64());
                println!("save_copy_bytes={}", summary.bytes_written);
            }
            SaveAsOutcome::Cancelled => return Err("save copy unexpectedly cancelled".into()),
        }
    }
    // Quarry writes uncompressed text only, so a gzip-named destination is refused.
    let refused = options.temp.join("refused-copy.csv.gz");
    match session.start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &refused) {
        Ok(job) => {
            job.cancel();
            drop(job.wait());
            return Err("a .gz destination was accepted".into());
        }
        Err(error) => println!("gz_destination_refused={}", error_kind(&error)),
    }
    session.ensure_compressed_source_unchanged()?;
    println!("source_unchanged=true");
    println!("result=complete");
    drop(index);
    drop(session);
    finish_cleanup(&options.temp, &cleanup)
}
