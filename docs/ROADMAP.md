# Quarry Roadmap

The [public Quarry Roadmap project](https://github.com/users/danchamorro/projects/1)
tracks current priorities, status, and target releases. Individual
[issues](https://github.com/danchamorro/quarry/issues) hold scope and acceptance
criteria. This document records product direction; it is not a second task board.

Quarry focuses on correct, responsive editing of delimited files larger than RAM.
New workflows must preserve bounded memory, explicit progress and cancellation,
source safety, and useful keyboard and accessibility behavior. The existing egui
desktop application remains the main application.

## Current foundation

The macOS app supports progressive opening, navigation, cell and header editing,
Find/Replace, filtering and export, column transformations, row deletion,
duplicate removal, several sort modes, and guarded saving. See the
[User Guide](USER_GUIDE.md) for current-source behavior and
[GitHub Releases](https://github.com/danchamorro/quarry/releases) for the features
included in each packaged beta. A merged feature is not necessarily released yet.

The [roadmap history](ROADMAP_HISTORY.md) preserves the earlier phase checklists
and their benchmark and acceptance evidence. Release readiness continues to use
the [beta checklist](BETA_RELEASE_CHECKLIST.md).

## Post-beta sequence

The existing feature direction is:

1. [Append compatible CSV files](https://github.com/danchamorro/quarry/issues/54):
   combine row batches with an explicit compatibility contract and bounded work.
2. [Independent document tabs](https://github.com/danchamorro/quarry/issues/55):
   preserve each document's edits, history, jobs, and save state.
3. [A terminal frontend](https://github.com/danchamorro/quarry/issues/58): define
   a small useful first slice on the shared engine, then split implementation
   into focused issues.

This sequence expresses direction, not release dates. Use the project's
**Priority** and **Target release** fields for current scheduling. An
**Unscheduled** target makes no release commitment.

### Phase 6D: Date and time sorting (planned)

[Date/time sorting](https://github.com/danchamorro/quarry/issues/56) carries forward
unfinished Phase 6D. It needs explicit formats and agreed handling of timezones,
invalid values, and blanks before implementation. Ambiguous dates must not be
guessed silently; stable chronology and bounded external sorting remain required.

## Platform direction

[Linux desktop support](https://github.com/danchamorro/quarry/issues/57) is the
first platform-expansion priority. Define and validate a bounded desktop matrix
before claiming support. Linux core/CLI CI does not establish desktop acceptance.
Other platform expansion remains exploratory.

## Later ideas

Persistent indexes, richer encoding and malformed-file feedback, multi-column
sorting, row insertion, side-by-side comparison, and key-based joins remain
possible directions. They need concrete user benefits, scope, and scalability
criteria before becoming implementation commitments. The
[historical ideas list](ROADMAP_HISTORY.md#later-possibilities) retains the wider
set of possibilities.

## Planning and contribution workflow

Use the [Wiki](https://github.com/danchamorro/quarry/wiki) to find documentation
and the [contributing guide](CONTRIBUTING.md) to propose or implement work.
The project provides a Backlog table, an Active work board, and a Releases view
grouped by target release. Keep detailed checklists in issues and current status
in the project so updates have one home.
