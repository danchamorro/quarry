//! Release validation driver; pair with /usr/bin/time -l and independent byte oracles.
use quarry_core::{
    ColumnTransformation, HeaderMode, OpenOptions, RowInsertion, SaveAsOutcome, Session,
};
use std::{
    collections::BTreeMap,
    env, thread,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 4 {
        return Err("usage: insertion-benchmark SOURCE NEW_DESTINATION baseline|rows|columns|cancel-rows|cancel-columns".into());
    }
    let session = Session::open(
        &args[1],
        OpenOptions {
            delimiter: Some(b','),
            header_mode: HeaderMode::FirstRow,
            ..OpenOptions::default()
        },
    )?;
    let started = Instant::now();
    let job = match args[3].as_str() {
        "baseline" => {
            session.start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &args[2])?
        }
        "rows" => session.start_create_working_copy_inserting_rows(
            BTreeMap::new(),
            BTreeMap::new(),
            RowInsertion {
                position: 5_000_001,
                count: 10,
                width: 3,
            },
            &args[2],
        )?,
        "cancel-rows" => session.start_create_working_copy_inserting_rows(
            BTreeMap::new(),
            BTreeMap::new(),
            RowInsertion {
                position: 1,
                count: 10_000_000,
                width: 3,
            },
            &args[2],
        )?,
        "columns" | "cancel-columns" => session.start_create_working_copy(
            BTreeMap::new(),
            BTreeMap::new(),
            ColumnTransformation::Insert {
                source_width: 3,
                position: 1,
                count: 2,
                headers: Some(vec![b"New".to_vec(), Vec::new()]),
            },
            &args[2],
        )?,
        _ => return Err("unknown benchmark mode".into()),
    };
    let mut cancellation_requested = false;
    let mut saw_counting = false;
    let mut saw_writing = false;
    while !job.progress().done {
        let phase = job.progress();
        saw_counting |= phase.preparing && phase.preparation_bytes_scanned > 0;
        saw_writing |= !phase.preparing && phase.bytes_written > 0;
        if args[3].starts_with("cancel-")
            && !cancellation_requested
            && job.progress().bytes_written >= 1_048_576
        {
            job.cancel();
            cancellation_requested = true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let progress = job.progress();
    let outcome = job.wait()?;
    println!(
        "counted={} estimated={} saw_counting={} saw_writing={}",
        progress.preparation_bytes_scanned,
        progress.estimated_output_bytes,
        saw_counting,
        saw_writing
    );
    println!(
        "mode={} seconds={:.6} scanned={} written={} cancel_requested={} outcome={:?}",
        args[3],
        started.elapsed().as_secs_f64(),
        progress.bytes_scanned,
        progress.bytes_written,
        cancellation_requested,
        outcome
    );
    if args[3].starts_with("cancel-") {
        if !matches!(outcome, SaveAsOutcome::Cancelled) || std::path::Path::new(&args[2]).exists() {
            return Err("cancellation published output".into());
        }
    } else if !matches!(outcome, SaveAsOutcome::Complete(_)) {
        return Err("operation did not complete".into());
    }
    Ok(())
}
