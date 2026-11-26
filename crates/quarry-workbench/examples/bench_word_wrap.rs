//! Phase 5A-6 Adaptive Word Wrap benchmark: empirically measures whether wrap
//! layout cost scales with total file size vs visible viewport text, and proves
//! bounded per-cell (4 KiB) and per-frame (128 KiB) latency on hostile multi-MB cells.

use std::time::Instant;

use quarry_core::{IndexConfig, OpenOptions, Session};
use quarry_workbench::{
    DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES, GRID_FONT_SIZE, MAX_WRAP_CELL_BYTES, MAX_WRAPPED_LINES,
    RowWrapCache, RowWrapCacheKey, WordWrapPreferences, WrapFrameBudget, column_widths_fingerprint,
    measure_wrapped_cell, resolve_word_wrap,
};

fn build_synthetic_csv(path: &std::path::Path, target_bytes: usize) -> std::io::Result<usize> {
    use std::io::{BufWriter, Write};
    let file = std::fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(1024 * 1024, file);
    let header =
        "id,date,region,warehouse,carrier,sku,units,unit_price,total,status,category,notes\n";
    w.write_all(header.as_bytes())?;
    let mut written = header.len();
    let mut rows = 0usize;
    let notes = [
        "Standard palletized dispatch scheduled for regional cross-dock consolidation and barcode check",
        "Temperature-monitored container requiring cold-chain verification at arrival gate and dock B4",
        "Priority customs pre-clearance filed with electronic manifest and secondary inspection waiver",
        "Consolidated regional manifest with multi-stop delivery notes and receiver signature required",
    ];
    while written < target_bytes {
        rows += 1;
        let line = format!(
            "SHP-{:08},2026-06-15,North America,SEA-01,Pacific Freight,QR-1042,{},42.50,4250.00,Delivered,Logistics,\"{}\"\n",
            rows,
            10 + (rows % 200),
            notes[rows % notes.len()]
        );
        w.write_all(line.as_bytes())?;
        written += line.len();
    }
    w.flush()?;
    Ok(rows)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let col_widths: Vec<f32> = vec![
        120.0, 100.0, 130.0, 96.0, 130.0, 96.0, 76.0, 96.0, 104.0, 100.0, 110.0, 220.0,
    ];
    let width_fp = column_widths_fingerprint(col_widths.iter().copied().enumerate());

    println!(
        "=== 1. Wrap Cost vs File Size (40 visible rows x 12 columns, real Session::read_rows) ==="
    );
    println!(
        "{:<10} {:>10} {:>12} {:>14} {:>14} {:>16}",
        "Target", "Actual MB", "Rows", "Cold Wrap us", "Warm Hit us", "Auto-Disable"
    );

    let mut prefs = WordWrapPreferences::load(None);
    prefs.set_enabled(true)?;

    for (label, target_bytes) in [
        ("1 MB", 1_000_000usize),
        ("50 MB", 50_000_000),
        ("299 MB", 299_000_000),
        ("301 MB", 301_000_000),
    ] {
        let path = tmp
            .path()
            .join(format!("wrap_{}.csv", label.replace(' ', "_")));
        let rows_written = build_synthetic_csv(&path, target_bytes)?;
        let actual_len = std::fs::metadata(&path)?.len();
        let effective = resolve_word_wrap(&prefs, None, Some(actual_len));

        let session = Session::open(&path, OpenOptions::default())?;
        let index = session.start_indexing(IndexConfig::default())?.wait()?;
        let mid_row = (rows_written as u64 / 2).saturating_sub(20).max(1);
        let fetched = session.read_rows(&index, mid_row, 40)?;

        // Cold measurement across 40 fetched rows (average of 20 fresh cache runs).
        let iterations = 20u32;
        let t_cold = Instant::now();
        for _ in 0..iterations {
            let mut cache = RowWrapCache::default();
            for (idx, row) in fetched.iter().enumerate() {
                let key = RowWrapCacheKey {
                    row: mid_row + idx as u64,
                    source_generation: 1,
                    edit_revision: 0,
                    show_all_whitespace: false,
                    font_size_tenths: (GRID_FONT_SIZE * 10.0) as u16,
                    width_fingerprint: width_fp,
                };
                cache.get_or_measure(
                    key,
                    GRID_FONT_SIZE,
                    row.fields
                        .iter()
                        .enumerate()
                        .map(|(c, f)| (f.as_slice(), col_widths.get(c).copied().unwrap_or(124.0))),
                );
            }
        }
        let cold_us = t_cold.elapsed().as_secs_f64() * 1_000_000.0 / f64::from(iterations);

        // Warm cache lookup across 40 fetched rows (average of 1,000 runs).
        let mut warm_cache = RowWrapCache::default();
        for (idx, row) in fetched.iter().enumerate() {
            let key = RowWrapCacheKey {
                row: mid_row + idx as u64,
                source_generation: 1,
                edit_revision: 0,
                show_all_whitespace: false,
                font_size_tenths: (GRID_FONT_SIZE * 10.0) as u16,
                width_fingerprint: width_fp,
            };
            warm_cache.get_or_measure(
                key,
                GRID_FONT_SIZE,
                row.fields
                    .iter()
                    .enumerate()
                    .map(|(c, f)| (f.as_slice(), col_widths.get(c).copied().unwrap_or(124.0))),
            );
        }
        let warm_iters = 1000u32;
        let t_warm = Instant::now();
        for _ in 0..warm_iters {
            for idx in 0..fetched.len() {
                let key = RowWrapCacheKey {
                    row: mid_row + idx as u64,
                    source_generation: 1,
                    edit_revision: 0,
                    show_all_whitespace: false,
                    font_size_tenths: (GRID_FONT_SIZE * 10.0) as u16,
                    width_fingerprint: width_fp,
                };
                let _ = warm_cache.get(&key);
            }
        }
        let warm_us = t_warm.elapsed().as_secs_f64() * 1_000_000.0 / f64::from(warm_iters);

        println!(
            "{:<10} {:>10.2} {:>12} {:>14.1} {:>14.2} {:>16}",
            label,
            actual_len as f64 / 1_000_000.0,
            rows_written,
            cold_us,
            warm_us,
            if effective.is_auto_disabled() {
                "AutoDisabled"
            } else {
                "Active"
            }
        );
        // Clean up each file immediately so temp disk stays small.
        let _ = std::fs::remove_file(&path);
    }

    println!(
        "\n=== 2. Hostile Single-Line Cell Budget Scaling (40 rows x 12 cols, 4 KiB/cell & 128 KiB/frame) ==="
    );
    println!(
        "{:<12} {:>14} {:>16} {:>14} {:>14}",
        "Cell Size", "Per-Cell us", "40x12 Frame us", "Inspected B", "Frame Clamped"
    );
    for (label, cell_bytes) in [
        ("64 B", 64usize),
        ("4 KiB", 4 * 1024),
        ("64 KiB", 64 * 1024),
        ("1 MiB", 1024 * 1024),
        ("10 MiB", 10 * 1024 * 1024),
    ] {
        let cell = vec![b'x'; cell_bytes];
        let t_cell = Instant::now();
        let mut last_inspected = 0;
        for _ in 0..100 {
            let m = measure_wrapped_cell(
                &cell,
                160.0,
                GRID_FONT_SIZE,
                MAX_WRAPPED_LINES,
                MAX_WRAP_CELL_BYTES,
            );
            last_inspected = m.inspected_bytes;
        }
        let per_cell_us = t_cell.elapsed().as_secs_f64() * 1_000_000.0 / 100.0;

        let t_frame = Instant::now();
        let mut budget = WrapFrameBudget::default_frame();
        for _ in 0..(40 * 12) {
            if budget.try_consume(cell.len()) {
                let _ = measure_wrapped_cell(
                    &cell,
                    160.0,
                    GRID_FONT_SIZE,
                    MAX_WRAPPED_LINES,
                    MAX_WRAP_CELL_BYTES,
                );
            }
        }
        let frame_us = t_frame.elapsed().as_secs_f64() * 1_000_000.0;

        println!(
            "{:<12} {:>14.2} {:>16.1} {:>14} {:>14}",
            label,
            per_cell_us,
            frame_us,
            last_inspected,
            budget.clamped_cells()
        );
    }

    println!(
        "\nDefault auto-disable threshold: {} bytes ({} MB)",
        DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES,
        DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES / 1_000_000
    );
    Ok(())
}
