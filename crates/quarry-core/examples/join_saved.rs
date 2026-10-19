//! Release-mode probe for the first saved-file join slice.
//! Usage: cargo run -p quarry-core --release --example join_saved -- MAIN ADDITIONAL OUTPUT TEMP
//! Both inputs require UTF-8 headers and a unique first-column key.
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use quarry_core::{Encoding, JoinConfig, JoinInput, JoinJob, analyze_join};

fn finish<T: Send + 'static>(job: JoinJob<T>) -> Result<T, Box<dyn std::error::Error>> {
    let mut previous = None;
    while !job.progress().done {
        let progress = job.progress();
        if previous != Some(progress.phase) {
            eprintln!("{}", progress.phase.label());
            previous = Some(progress.phase);
        }
        thread::sleep(Duration::from_millis(20));
    }
    job.wait()?.ok_or_else(|| "join cancelled".into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if paths.len() != 4 {
        return Err("usage: join_saved MAIN ADDITIONAL OUTPUT TEMP".into());
    }
    let inputs = [paths[0].clone(), paths[1].clone()].map(|path| JoinInput {
        path,
        delimiter: Some(b','),
        has_header: true,
        encoding: Some(Encoding::Utf8),
        key_column: 0,
        expected_version: None,
    });
    let start = Instant::now();
    let plan = finish(analyze_join(
        inputs,
        paths[3].clone(),
        JoinConfig::default(),
    )?)?;
    println!("analysis_seconds={:.6}", start.elapsed().as_secs_f64());
    println!("input_rows={:?}", plan.analysis().input_rows);
    println!("matching_pairs={}", plan.analysis().matching_pairs);
    println!("output_rows={}", plan.analysis().output_rows);
    println!("output_bytes={}", plan.analysis().output_bytes);
    let result = finish(plan.start_write(paths[2].clone())?)?;
    println!("total_seconds={:.6}", start.elapsed().as_secs_f64());
    println!("actual_rows={}", result.data_rows);
    println!("actual_bytes={}", result.bytes_written);
    println!("peak_temporary_bytes={}", result.peak_temporary_bytes);
    if let Some(warning) = result.cleanup_warning {
        return Err(warning.into());
    }
    Ok(())
}
