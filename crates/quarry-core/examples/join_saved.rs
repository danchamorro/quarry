//! Release-mode probe for the first saved-file join slice.
//! Usage: cargo run -p quarry-core --release --example join_saved -- MAIN ADDITIONAL OUTPUT TEMP
//! Both inputs require UTF-8 headers and use the first column as their key.
//! Pass --acknowledge-expansion to create a many-to-many result.
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
    let mut paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let acknowledge = paths
        .last()
        .is_some_and(|path| path == std::path::Path::new("--acknowledge-expansion"));
    if acknowledge {
        paths.pop();
    }
    if paths.len() != 4 {
        return Err(
            "usage: join_saved MAIN ADDITIONAL OUTPUT TEMP [--acknowledge-expansion]".into(),
        );
    }
    let inputs = [paths[0].clone(), paths[1].clone()].map(|path| JoinInput {
        path,
        delimiter: Some(b','),
        has_header: true,
        encoding: Some(Encoding::Utf8),
        key_columns: vec![0],
        expected_version: None,
        require_unique: false,
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
    println!("duplicate_groups={:?}", plan.analysis().duplicate_groups);
    println!("many_to_many_pairs={}", plan.analysis().many_to_many_pairs);
    println!("output_bytes={}", plan.analysis().output_bytes);
    println!(
        "additional_temporary_bytes={}",
        plan.analysis().additional_temporary_bytes
    );
    let result = finish(plan.start_write_acknowledged(paths[2].clone(), acknowledge)?)?;
    println!("total_seconds={:.6}", start.elapsed().as_secs_f64());
    println!("actual_rows={}", result.data_rows);
    println!("actual_bytes={}", result.bytes_written);
    println!("peak_temporary_bytes={}", result.peak_temporary_bytes);
    println!("peak_group_buffer_bytes={}", result.peak_group_buffer_bytes);
    if let Some(warning) = result.cleanup_warning {
        return Err(warning.into());
    }
    Ok(())
}
