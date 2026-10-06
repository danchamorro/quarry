# Quarry Roadmap

The [Quarry Roadmap project](https://github.com/users/danchamorro/projects/1) is
the authoritative home for **all future roadmap items**, including exploratory
ideas, engineering investigations, planned features, priorities, and release
targets. Each item has a linked [issue](https://github.com/danchamorro/quarry/issues)
for its problem, scope, design questions, and acceptance or discovery criteria.

Create and update roadmap items there. Do not maintain a separate future-work
list or ordered backlog in this file, the Wiki, or another repository document.

## Product focus

Quarry focuses on correct, responsive editing of delimited files larger than RAM.
New workflows must preserve bounded memory, explicit progress and cancellation,
source safety, and useful keyboard and accessibility behavior. The existing egui
desktop application remains the main application.

## Using the roadmap

- **Backlog:** all tracked work, including ideas awaiting prioritization or design.
- **Ideas:** exploratory issues labeled `idea`; recording an idea is not a promise
  to implement it. Agree on scope before moving it to Ready, and remove the label
  when it becomes an accepted implementation item.
- **Active work:** the workflow board from Backlog through Done.
- **Releases:** work grouped by Target release. **Unscheduled** means no release
  commitment; a merged change may ship in a later packaged release.

The project's Priority, Area, Target release, and Status fields hold current
planning decisions. Maintainers triage new issues, split broad investigations
into focused implementation issues, and link pull requests to the work they
complete. See [Contributing](CONTRIBUTING.md#proposing-and-tracking-work).

## Documentation and evidence

The [User Guide](USER_GUIDE.md) documents current-source behavior, and
[GitHub Releases](https://github.com/danchamorro/quarry/releases) defines what is
included in each packaged build. Architecture documents and ADRs retain design
rationale and link to issues when future investigation is relevant. Release
acceptance evidence remains in the [beta checklist](BETA_RELEASE_CHECKLIST.md).

[Roadmap history](ROADMAP_HISTORY.md) preserves the earlier phase checklists and
benchmark evidence as an archival snapshot. Its unfinished items and later ideas
have been captured in Projects; the archive is not an active planning list.

## Post-beta sequence

Follow priorities and dependencies in the [live project](https://github.com/users/danchamorro/projects/1).
This section remains a navigation destination for older links, not a second
ordered feature list.

### Phase 6D: Date and time sorting (planned)

The former Phase 6D is tracked in [issue #56](https://github.com/danchamorro/quarry/issues/56).
Its scope, parsing decisions, and acceptance criteria belong in that issue.

## Platform direction

Platform investigations and scheduling are tracked in the
[project](https://github.com/users/danchamorro/projects/1). The
[beta platform matrix](BETA_RELEASE_CHECKLIST.md#platform-matrix) records what
has actually been validated; a roadmap item does not establish platform support.
