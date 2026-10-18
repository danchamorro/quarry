# Collapse repeated spaces validation — 2026-10-17

The new cleanup operation was tested inside VMPal's macOS VM. Only runs of
U+0020 ordinary spaces collapse. Other whitespace, unselected columns, and
headers retain their values.

## Environment and method

- Apple M3 Max (Virtual), 16 GiB RAM.
- macOS 27.0.1 (26A434), rustc 1.88.0 (6b00bc388 2025-06-23).
- Optimized release build; one standalone core helper process per input.
- The helper opened a comma-delimited file with a header, called
  `Session::start_create_cleaned_working_copy` with column 0 and
  `CleanupOperation::CollapseSpaces`, and waited for completion.
- Files were generated locally and hashed immediately before the run. File
  cache was warm/uncontrolled; these are not cold-storage measurements.
- The header was `city,untouched`. Every data row was
  `Los  Angeles,keep  spaces`. Expected output was
  `Los Angeles,keep  spaces`.
- Wall time covers opening and completing the cleanup job. Peak resident memory
  comes from macOS `/usr/bin/time -l` and covers the helper, not the GUI.

## Results

| Input bytes | Data rows / changed cells | Seconds | Peak RSS bytes |
| ---: | ---: | ---: | ---: |
| 16,777,217 | 645,277 | 0.196890 | 4,358,144 |
| 536,870,921 | 20,648,881 | 4.259821 | 4,358,144 |

Every output byte was checked against the expected header and repeated row.
Both source hashes remained unchanged. These runs support bounded memory for
this workload; they are not a speed comparison with the previous application
or a larger-than-RAM validation.

## Behavioral checks

- Both missing UI controls were reproduced with failing regression checks before
  implementation: the renamed Trim label and the Collapse option.
- All 420 workspace tests, rustfmt, Clippy with warnings denied, and the release
  build passed.
- The GUI regression selects Collapse through accessibility and checks preview,
  pending edits, changed-cell count, Undo/Redo, Save As, and whitespace metadata.
- VMPal native checks confirmed both labels fit the dialog, Collapse updates
  `Los  Angeles` to `Los Angeles`, and its subtle-view triangle disappears.
  Undo restores the triangle; Redo removes it. Clipboard text is the raw value.
- The renamed Trim action removes remaining edge spaces. Tabs and embedded line
  breaks survive both operations. Native Save and menu-driven Save As outputs
  were checked byte for byte; Save As preserved its separate synthetic source.
