# Filtering unsaved edits validation

Environment: VMPal macOS VM 1, macOS 27.0.1 (26A434), Apple M3 Max virtual CPU, 16 GiB RAM; Rust 1.88.0, release profile. Synthetic CSV only.

The input had a header and 800,000 data records (99,888,907 bytes). Each record was id,state,payload: IDs 0 through 799999, alternating keep / drop, and 112 ASCII x characters. The filter was state Equals keep. The edited run changed physical data rows 1, 1001, ..., 799001 to drop in an immutable sparse snapshot. The timing includes the filter scan and reading its last three matches. Inputs were warm after generation.

| Run | No edits | 800 edits | Peak RSS with edits |
| --- | ---: | ---: | ---: |
| 1 | 0.070131 s | 0.096114 s | 24,068,096 bytes |
| 2 | 0.072235 s | 0.096651 s | 24,084,480 bytes |
| 3 | 0.070319 s | 0.096427 s | 24,068,096 bytes |

All raw runs returned 400,000 matches; edited runs returned 399,200. The last three physical rows were 799995, 799997 and 799999 in both modes. Checkpoints occupied 9,600,000 bytes and 9,580,800 bytes respectively, below the 16 MiB budget. RSS was measured with macOS /usr/bin/time -l. This is a local synthetic throughput check, not a guarantee for every file or storage device.

To reproduce, generate the records above with LF endings and invoke Session::start_filter_with_edits using HeaderMode::FirstRow, FilterQuery::single(1, FilterOperator::Equals, b"keep".to_vec()), and the stated edit map. Wait for the index, then call read_filtered_rows(&index, index.matches_found() - 3, 3). Time each fresh process and assert the counts above. The ordinary start_filter path delegates to this same implementation with an empty snapshot.

Native VM validation used a separate three-record fixture. An unsaved change from drop to keep appeared in the filtered results; changing it back removed that row; Undo restored it. Export contained the edited value while the source bytes stayed unchanged. Automated regressions additionally cover header renames, exact numeric matching, case matching, missing fields, compacted checkpoints, stale sources, value history, and whole-file Save As.
