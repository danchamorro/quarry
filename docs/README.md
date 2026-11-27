# Quarry documentation

Map of the maintained documentation in this directory. Product claims and their
evidence live in the [v1.0 Claim Ledger](V1_CLAIM_LEDGER.md); per-release notes
live in [`releases/`](releases/) and [`CHANGELOG.md`](../CHANGELOG.md).

## Start here

- [User Guide](USER_GUIDE.md) — opening files, editing, filters, sorting,
  sections, bookmarks, Live Watch Mode, shortcuts, and safety guards.
- [Linux & Omarchy Guide](LINUX_AND_OMARCHY.md) — packaging, native
  Wayland/Hyprland, live theme sync, `tmpfs` scratch routing, AT-SPI.
- [macOS Packaging Guide](MACOS_PACKAGING.md) — bundling, verification,
  rollback, signing, notarization, and release artifacts.
- [Beta Tester Guide](BETA_TESTER_GUIDE.md) and [Beta Release
  Notes](BETA_RELEASE_NOTES.md) — current prerelease, compatibility, and how to
  report problems.

## Product and engineering

- [Product Requirements](PRD.md) — version-by-version scope and requirements.
- [Roadmap](ROADMAP.md) — shipped highlights on `main`, upcoming v1.0
  milestones, and pointer to open GitHub Issues.
- [Design System](DESIGN.md) — palettes, typography, layout, and components.
- [Engineering Principles](ENGINEERING_PRINCIPLES.md) — the rules every change
  is measured against.
- [Architecture](ARCHITECTURE.md) — streaming engine, positional I/O, SIMD
  indexing, sidecar cache, bounded memory, Live Watch Mode, and packaging.
- [Architecture Decision Records](adr/) — `ADR 0001` through `ADR 0016`.
- [Contributing](CONTRIBUTING.md) — repository layout, validation commands, and
  review expectations.

## Verification and evidence

- [v1.0 Claim Ledger](V1_CLAIM_LEDGER.md) — claim contracts with evidence and
  owner decision gates.
- [Benchmark Datasets](BENCHMARK_DATASETS.md) and [`benchmarks/`](benchmarks/) —
  dataset setup and dated measurement reports.
- [Inspect File validation](INSPECTION_VALIDATION.md) — inspection and recovery
  copy acceptance evidence.
- [Beta Release Checklist](BETA_RELEASE_CHECKLIST.md) — per-archive acceptance
  records.
- [GPUI frontend validation](../apps/quarry-gpui/VALIDATION.md) — frontend
  hardening evidence.

## Historical records

- [Roadmap History](ROADMAP_HISTORY.md), [Beta Release
  History](BETA_RELEASE_HISTORY.md), and [Pre-Beta
  Checklist](PRE_BETA_CHECKLIST.md) — superseded planning and acceptance
  records, kept for traceability.
