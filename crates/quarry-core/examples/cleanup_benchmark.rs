use quarry_core::{
    CleanupOperation, CleanupOutcome, ColumnCleanup, HeaderMode, OpenOptions, Session,
};
use std::{
    env, thread,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().collect();
    if args.len() < 5 {
        return Err("usage: cleanup_benchmark INPUT OUTPUT trim|upper|lower|fill COLUMN [cancel_after_written_bytes]".into());
    }
    let column: usize = args[4]
        .parse::<usize>()?
        .checked_sub(1)
        .ok_or("column must be positive")?;
    let operation = match args[3].as_str() {
        "trim" => CleanupOperation::Trim,
        "upper" => CleanupOperation::Uppercase,
        "lower" => CleanupOperation::Lowercase,
        "fill" => CleanupOperation::FillEmpty(b"filled".to_vec()),
        _ => return Err("unknown cleanup operation".into()),
    };
    let cancel_after = args.get(5).map(|s| s.parse::<u64>()).transpose()?;
    let session = Session::open(
        &args[1],
        OpenOptions {
            header_mode: HeaderMode::FirstRow,
            rows: 1,
            ..OpenOptions::default()
        },
    )?;
    let started = Instant::now();
    let job = session.start_create_cleaned_working_copy(
        Default::default(),
        Default::default(),
        ColumnCleanup {
            columns: vec![column],
            operation,
        },
        &args[2],
    )?;
    let mut cancelled = false;
    let mut max_written = 0;
    loop {
        let p = job.progress();
        max_written = max_written.max(p.bytes_written);
        if !cancelled && cancel_after.is_some_and(|limit| p.bytes_written >= limit) {
            job.cancel();
            cancelled = true;
        }
        if p.done {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    match job.wait()? {
        CleanupOutcome::Complete(result) => println!(
            "complete changed_cells={} bytes_written={} seconds={:.3}",
            result.changed_cells,
            result.bytes_written,
            started.elapsed().as_secs_f64()
        ),
        CleanupOutcome::Unchanged => {
            println!("unchanged seconds={:.3}", started.elapsed().as_secs_f64())
        }
        CleanupOutcome::Cancelled => println!(
            "cancelled observed_written_bytes={} seconds={:.3}",
            max_written,
            started.elapsed().as_secs_f64()
        ),
    }
    Ok(())
}
