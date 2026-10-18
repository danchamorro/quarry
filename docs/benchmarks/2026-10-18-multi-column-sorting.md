# Multi-column sorting VM validation (2026-10-18)

Issue: [#64](https://github.com/danchamorro/quarry/issues/64).
Baseline: 95b8eb70dca800b6858144bc47205fe6364f4429.
Candidate: the multi-column sorting implementation accompanying this document.

## Environment and workload
- VMPal macOS VM 1; macOS 27.0.1 (26A434).
- Apple M3 Max (Virtual), 16 GiB RAM; Rust 1.88.0.
- Optimized release builds, warm filesystem cache, local APFS storage.
- Synthetic seed-64 CSV, 12 columns, 100,002,890 bytes, 470,809 data rows plus header.
- Three repetitions per build, alternating baseline and candidate for single-key runs.
- No cold-cache or 50 GB throughput claim is made.

## Results

| Operation | Times (seconds) | Median | Peak RSS |
| --- | --- | --- | --- |
| Baseline, column 2 text ascending | 0.577, 0.603, 0.553 | 0.577 | 28.72–28.80 MiB through sort |
| Candidate, same single-key operation | 0.576, 0.650, 0.587 | 0.587 | 28.77–28.80 MiB through sort |
| Candidate, columns 2 asc / 3 desc / 4 asc, all text | 0.651, 0.657, 0.650 | 0.651 | 28.125 MiB process maximum |

Single-key output was byte-identical to the baseline. The small timing
difference is within the variability of these runs. The three-key run created
9 runs and 1 merge pass; peak temporary bytes were 236,731,003 (225.76 MiB).
The engine reported preserved record multiset and stable ties.

An independent Python csv.reader pass verified the three-component order
(with the middle component reversed), header, row count, and SHA-256 row
multiset sum against the source. It did not use the engine's encoded keys.
The forced multi-pass unit test independently sorts decoded text/integer/date
tuples and checks exact output including tied row IDs and pending edits.

## Reproduction
Generate the same input with the release CLI:
```sh
cargo build --workspace --release --locked
target/release/quarry generate --size 100MB --columns 12 --seed 64 --output /tmp/multi-input.csv
/usr/bin/time -l target/release/quarry sort-save-as /tmp/multi-input.csv /tmp/single-output.csv --column 2 --order asc --header first-row --cache-state warm
```

Build the following temporary example in crates/quarry-core/examples/
as multi-sort-validation.rs, then run it against the generated file:
```rust
use quarry_core::{
    CaseSensitivity, HeaderMode, OpenOptions, Session, SortDirection, SortMode, SortOutcome,
    SortPlan, SortSpec,
};
use std::{collections::BTreeMap, env, time::Instant};
fn main() {
    let args: Vec<_> = env::args().collect();
    let session = Session::open(
        &args[1],
        OpenOptions {
            header_mode: HeaderMode::FirstRow,
            ..Default::default()
        },
    )
    .unwrap();
    session
        .start_indexing(Default::default())
        .unwrap()
        .wait()
        .unwrap();
    let keys = vec![
        (1, SortDirection::Ascending),
        (2, SortDirection::Descending),
        (3, SortDirection::Ascending),
    ]
    .into_iter()
    .map(|(column, direction)| SortSpec {
        column,
        direction,
        mode: SortMode::Text,
        case_sensitivity: CaseSensitivity::Sensitive,
    })
    .collect();
    let start = Instant::now();
    let result = session
        .start_create_sorted_working_copy(
            BTreeMap::new(),
            BTreeMap::new(),
            SortPlan { keys },
            &args[2],
        )
        .unwrap()
        .wait()
        .unwrap();
    if let SortOutcome::Complete(s) = result {
        println!(
            "seconds={:.3} rows={} runs={} passes={} peak_temp={} verified={} stable={}",
            start.elapsed().as_secs_f64(),
            s.rows_sorted,
            s.runs_created,
            s.merge_passes,
            s.peak_temporary_bytes,
            s.record_multiset_verified,
            s.stable_ties_verified
        );
    } else {
        panic!("cancelled")
    }
}
```

```sh
cargo build -p quarry-core --example multi-sort-validation --release --locked
/usr/bin/time -l target/release/examples/multi-sort-validation /tmp/multi-input.csv /tmp/multi-output.csv
```

Use a fresh output pathname for every repetition. Baseline single-key runs use
the same CLI command from the baseline revision above. The temporary example is
a validation harness, not a new supported CLI interface.

## Native and automated checks
The native egui release candidate was exercised through VMPal: open a
synthetic CSV, add a second level, choose its column and Number type, set
descending order, and apply. Account ascending / amount descending produced
IDs 3,2,4,1,5; equal amount rows 2 and 4 remained stable. Undo restored IDs
1,2,3,4,5 and Redo restored the sorted order.

The full workspace suite passed 428 tests; Clippy passed with warnings denied.
Coverage includes per-key controls and reordering, pending edits, one-step
undo/redo, Save As source preservation, exact numbers, explicit dates, NUL and
prefix key ordering, missing fields, invalid secondary values, cancellation,
disk/key/memory bounds, duplicate detection, and existing single-key behavior.
