# ADR 0003: Select egui for the production UI

**Status:** Accepted
**Date:** 2026-08-14

## Context

Quarry is macOS-first, but its UI must keep 10 GB files responsive, stay under
the initial 500 MiB viewing target, support keyboard navigation and VoiceOver,
and keep the Rust engine independent. Phase 2 built egui and native AppKit
prototypes over the same engine with 100-row viewport bounds.

## Evidence

| Measure | egui | AppKit |
|---|---:|---:|
| First UI update/window on the 11.33 GiB file | 187.423 ms | 151.908 ms |
| Maximum resident set size | 137.0 MiB | 114.1 MiB |
| Peak physical footprint reported by macOS | 369.2 MiB | 43.0 MiB |
| Release binary | 5.18 MiB | 0.57 MiB |
| Normal dependency packages | 273 | 17 |
| Prototype source | 669 lines | 933 lines |
| Source lines containing `unsafe` | 0 | 21 |
| Accessible data model | Visible headers and cells | One text area |

Both candidates stayed responsive during indexing, implemented open,
progress/cancellation, previous/next, row jump, and Page Up/Page Down, and kept
viewport reads below 4 ms in the interaction smoke. A synthesized 10-page
scroll burst completed in 1.073 seconds for egui and 1.026 seconds for AppKit;
the harness includes a roughly one-second event stream, so this establishes the
absence of a stall rather than a meaningful renderer-speed difference.

AppKit wins startup, memory, binary size, and dependency count. Those margins
do not change the user-visible result at the current target: both show useful
rows in well under one second and remain below 500 MiB. The AppKit prototype is
larger despite using a simpler text viewport. Reaching feature parity would
require a cell-aware native grid and more Objective-C integration. egui already
provides a virtualized table, per-cell accessibility, and a safe Rust path.

## Decision

Use egui as Quarry's production UI and keep the Rust engine UI-independent.
Treat the measured egui overhead as a budget, not permission for unbounded
growth.

## Consequences

- Phase 3 will evolve `quarry-egui` into the viewer alpha.
- The UI keeps 100-row requests and delegates indexing and parsing to
  `quarry-core`.
- Release checks retain the 500 MiB viewing ceiling and add continuous-scroll
  frame pacing once the Phase 3 grid supports file-level scrolling.
- VoiceOver must continue exposing visible headers and cells; regressions are
  release blockers.
- Reconsider a native shell or grid only if lower-memory hardware, frame pacing,
  native text behavior, or accessibility measurements miss their targets.

Detailed runs are recorded in the [egui](../benchmarks/2026-08-14-egui-spike.md)
and [AppKit](../benchmarks/2026-08-14-appkit-spike.md) benchmark notes.

