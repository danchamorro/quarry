//! Release-mode acceptance probe for the complete join contract.
//! Inputs/output/temp are positional. Optional flags control modes, keys,
//! delimiters, bounded run/group targets and real cancellation by row progress.
use quarry_core::{Encoding, JoinConfig, JoinInput, JoinJob, JoinMode, JoinPhase, analyze_join};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

struct Options {
    paths: Vec<PathBuf>,
    config: JoinConfig,
    delimiters: [u8; 2],
    keys: usize,
    cancel_rows: Option<u64>,
    acknowledge: bool,
}
fn options() -> Result<Options, Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() < 4 {
        return Err("usage: join_acceptance MAIN ADDITIONAL OUTPUT TEMP [flags]".into());
    }
    let mut options = Options {
        paths: args[..4].iter().map(PathBuf::from).collect(),
        config: JoinConfig::default(),
        delimiters: [b','; 2],
        keys: 1,
        cancel_rows: None,
        acknowledge: false,
    };
    let mut index = 4;
    while index < args.len() {
        let flag = args[index].to_str().ok_or("non-text benchmark flag")?;
        if flag == "--acknowledge-expansion" {
            options.acknowledge = true;
            index += 1;
            continue;
        }
        index += 1;
        let value = args
            .get(index)
            .and_then(|arg| arg.to_str())
            .ok_or("missing flag value")?;
        match flag {
            "--mode" => {
                options.config.mode = match value {
                    "left" => JoinMode::Left,
                    "inner" => JoinMode::Inner,
                    "right" => JoinMode::Right,
                    "full" => JoinMode::Full,
                    _ => return Err("unknown join mode".into()),
                }
            }
            "--keys" => options.keys = value.parse()?,
            "--run-bytes" => options.config.run_memory_bytes = value.parse()?,
            "--group-bytes" => options.config.group_memory_bytes = value.parse()?,
            "--cancel-after-rows" => options.cancel_rows = Some(value.parse()?),
            "--main-delimiter" | "--additional-delimiter" => {
                let side = usize::from(flag == "--additional-delimiter");
                options.delimiters[side] = match value {
                    "comma" => b',',
                    "tab" => b'\t',
                    _ => return Err("unknown delimiter".into()),
                };
            }
            _ => return Err(format!("unknown flag {flag}").into()),
        }
        index += 1;
    }
    Ok(options)
}
fn finish<T: Send + 'static>(
    job: JoinJob<T>,
    cancel_rows: Option<u64>,
) -> Result<Option<T>, Box<dyn std::error::Error>> {
    let mut previous = None;
    let mut requested = None;
    let mut observed_peak = 0;
    loop {
        let progress = job.progress();
        observed_peak = observed_peak.max(progress.temporary_bytes);
        if previous != Some(progress.phase) {
            eprintln!("phase={}", progress.phase.label());
            previous = Some(progress.phase);
        }
        if !progress.done
            && requested.is_none()
            && progress.phase == JoinPhase::Creating
            && cancel_rows.is_some_and(|threshold| progress.records >= threshold)
        {
            job.cancel();
            requested = Some(Instant::now());
            eprintln!("cancel_requested_after_rows={}", progress.records);
        }
        if progress.done {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let result = job.wait()?;
    println!("observed_peak_temporary_bytes={observed_peak}");
    if let Some(requested) = requested {
        println!(
            "cancel_latency_seconds={:.9}",
            requested.elapsed().as_secs_f64()
        );
    }
    if cancel_rows.is_some() && requested.is_none() {
        return Err("benchmark finished before requested cancellation point".into());
    }
    Ok(result)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = options()?;
    let inputs = std::array::from_fn(|side| JoinInput {
        path: options.paths[side].clone(),
        delimiter: Some(options.delimiters[side]),
        has_header: true,
        encoding: Some(Encoding::Utf8),
        key_columns: (0..options.keys).collect(),
        expected_version: None,
        require_unique: false,
    });
    let start = Instant::now();
    let plan = finish(
        analyze_join(inputs, options.paths[3].clone(), options.config)?,
        None,
    )?
    .ok_or("analysis unexpectedly cancelled")?;
    println!("analysis_seconds={:.9}", start.elapsed().as_secs_f64());
    println!("input_rows_main={}", plan.analysis().input_rows[0]);
    println!("input_rows_additional={}", plan.analysis().input_rows[1]);
    println!("matching_pairs={}", plan.analysis().matching_pairs);
    println!("projected_output_rows={}", plan.analysis().output_rows);
    println!("projected_output_bytes={}", plan.analysis().output_bytes);
    println!(
        "additional_temporary_bytes={}",
        plan.analysis().additional_temporary_bytes
    );
    let result = finish(
        plan.start_write_acknowledged(options.paths[2].clone(), options.acknowledge)?,
        options.cancel_rows,
    )?;
    println!("total_seconds={:.9}", start.elapsed().as_secs_f64());
    match result {
        Some(summary) => {
            println!("result=complete");
            println!("actual_rows={}", summary.data_rows);
            println!("actual_bytes={}", summary.bytes_written);
            println!("peak_temporary_bytes={}", summary.peak_temporary_bytes);
            println!(
                "peak_group_buffer_bytes={}",
                summary.peak_group_buffer_bytes
            );
            if let Some(warning) = summary.cleanup_warning {
                return Err(warning.into());
            }
        }
        None => println!("result=cancelled"),
    }
    Ok(())
}
