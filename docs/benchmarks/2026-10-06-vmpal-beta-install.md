# Build 127 browser download and installation in VMPal

Recorded October 6, 2026. The published beta passed browser download, Finder
installation, normal first launch and a basic CSV-opening check in a VMPal
guest. This supplements the [build 127 report](2026-10-05-beta-build127.md);
it does not establish complete platform acceptance.

## Candidate and environment

| Field | Value |
|---|---|
| Release | [v0.1.0-beta.1](https://github.com/danchamorro/quarry/releases/tag/v0.1.0-beta.1) |
| Version/build | 0.1.0 (127) |
| Source | `30fee5d62ff23bb4803f73fbe5b14129162efb43`, clean |
| Guest | macOS 27.0.1 (26A434), Apple Silicon (`arm64`) |
| Automation | VMPal MCP guest execution, screenshots and input |
| Archive | `Quarry-0.1.0-beta.1-macos-arm64.zip`, 3,622,188 bytes |
| Archive SHA-256 | `f8fac678f75343bd5eef856826a274ed965928881a8aa02b04dab7452af3cc44` |

There was no `/Applications/Quarry.app` before installation. The VM already
existed and had Chrome installed; a newly provisioned environment and absence
of earlier Gatekeeper assessment history were not established.

## Download, installation and trust

1. Downloaded the official GitHub release asset using Chrome inside the guest.
   The archive's size and SHA-256 matched the published candidate, and its
   browser quarantine attribute was present.
2. Extracted the ZIP with Archive Utility and copied `Quarry.app` into
   Applications using Finder. The installed bundle retained quarantine.
3. Verified the installed version, build, clean source revision and executable
   location. `codesign --verify --deep --strict` passed. The signature used
   Developer ID Application: Daniel Chamorro (H599JA54R8), hardened runtime,
   and a stapled notarization ticket.
4. `spctl --assess --type execute` accepted the app as `Notarized Developer ID`.
5. Opened the installed app normally through LaunchServices. macOS displayed
   its downloaded-app confirmation and reported that Apple's malicious-software
   check found none. Choosing **Open** launched Quarry and displayed its grid.
   No quarantine attribute was removed and no security protection was bypassed.

Guest commands ran without elevation. With owner approval, VMPal Tools could
access the guest's Downloads folder. This did not change host permissions or
replace the host's installed application.

## Basic CSV opening

Opened a synthetic CSV containing four data records and three columns through
the installed app. It included a quoted comma, accented and CJK text, a blank
field and an embedded newline. Quarry displayed four rows, three columns and
**Index complete**. The source SHA-256 remained unchanged:
`40de08977bc2faeae863b676d3448963cf0ee6416b3fac4a04c5a2d29de9ea57`.

The CJK glyph appeared as a fallback box, so this is not a complete Unicode
font-rendering pass. Auto-fit controls were exercised, but their outcome was
not established. No edit/save, full connected workflow, offline first launch
or macOS 26 acceptance is claimed for this run.

The public build predates the later About/feedback panel, complete cell grid
lines and expanded long-cell previews. This result applies to build 127 only;
a new package needs its own recorded validation. Raw logs and screenshots are
retained locally under `target/vmpal-install/`. Remaining acceptance is tracked
in the [beta checklist](../BETA_RELEASE_CHECKLIST.md).
