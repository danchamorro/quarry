# Quarry Engineering Principles

## 1. Huge files are the normal case
Do not design for 100 MB and hope 10 GB works later. The large-file path is the primary path.

## 2. RAM is a budget, not a mirror of file size
Memory use must be intentionally bounded. A larger file may require more time or disk I/O, but not equivalent resident memory.

## 3. First useful pixels beat complete preprocessing
Show the user useful rows as soon as they can be parsed. Indexing continues behind the interface.

## 4. Never freeze the UI
Long work runs away from the UI thread, reports progress where meaningful, and supports cancellation.

## 5. Benchmark everything that matters
Performance claims require reproducible datasets, hardware details, build mode, and cache conditions. A meaningful regression is a bug.

## 6. Correctness before cleverness
CSV edge cases are real. Never trade silent data corruption for a benchmark win.

## 7. Optimize measured bottlenecks
Profile before introducing unsafe code, custom allocators, SIMD, GPU rendering, or complicated concurrency.

## 8. Keep the engine independent
The Rust engine should not know whether the UI is SwiftUI, AppKit, egui, Slint, or something else.

## 9. Bound every cache and queue
If a structure can grow with the number of rows, matches, or chunks, it needs a budget, paging strategy, or explicit justification.

## 10. Prefer streaming transformations
Split/join/filter/export should operate as pipelines instead of materializing an edited copy of the dataset in memory.

## 11. Data safety is part of performance engineering
A fast tool that corrupts a 15 GB customer file is useless. Early writing should default to new output files and robust failure handling.

## 12. Make performance observable
Developer diagnostics should expose enough information to understand bytes scanned, rows parsed, cache usage, throughput, and background jobs.

## 13. Avoid framework religion
Use a component because measurements and maintainability justify it, not because a platform vendor or language community says it is the default.

## 14. Keep the product narrow until the core is exceptional
Do not bury the defining performance work under a backlog of generic editor features.

## 15. The Quarry test
Before merging a major feature, ask:

> What happens when the source file is 50 GB and the machine has 16 GB of RAM?

If the answer is “it needs the whole file in memory,” the design is unfinished.
