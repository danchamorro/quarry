//! Deterministic faults around real public join jobs. Compiled only for tests.
//! Hooks are scoped to the initiating thread and copied into its join worker.
use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use crate::join::*;
use crate::{Encoding, QuarryError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Phase(JoinPhase),
    Merge,
    SpillWrite,
    SpillFlush,
    ExpandingPairs,
    RestoringOrder,
    WritingRow,
    OutputWrite,
    OutputFlush,
    OutputSync,
    DestinationPinned,
    BeforeSourceGuard,
    BeforePublication,
    AfterPublication,
}

enum Action {
    Pause {
        reached: Mutex<Option<mpsc::Sender<()>>>,
        resume: Mutex<mpsc::Receiver<()>>,
        failure: Option<bool>,
    },
    Fail {
        disk_full: bool,
    },
}

pub(crate) struct Injection {
    event: Event,
    skip: usize,
    seen: AtomicUsize,
    fired: AtomicBool,
    action: Action,
}

thread_local! {
    static ACTIVE: RefCell<Option<Arc<Injection>>> = const { RefCell::new(None) };
}

pub(crate) struct Scope(Option<Arc<Injection>>);
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|active| *active.borrow_mut() = self.0.take());
    }
}
pub(crate) fn capture() -> Option<Arc<Injection>> {
    ACTIVE.with(|active| active.borrow().clone())
}
pub(crate) fn install(injection: Option<Arc<Injection>>) -> Scope {
    Scope(ACTIVE.with(|active| active.replace(injection)))
}

pub(crate) fn point(event: Event) -> io::Result<()> {
    let Some(injection) = capture() else {
        return Ok(());
    };
    if injection.event != event
        || injection.seen.fetch_add(1, Ordering::AcqRel) < injection.skip
        || injection.fired.swap(true, Ordering::AcqRel)
    {
        return Ok(());
    }
    match &injection.action {
        Action::Pause {
            reached,
            resume,
            failure,
        } => {
            if let Some(sender) = reached.lock().unwrap().take() {
                let _ = sender.send(());
            }
            // A failing test must never strand a joined worker indefinitely.
            let _ = resume.lock().unwrap().recv_timeout(Duration::from_secs(10));
            failure.map_or(Ok(()), |disk_full| Err(io_failure(event, disk_full)))
        }
        Action::Fail { disk_full } => Err(io_failure(event, *disk_full)),
    }
}
fn io_failure(event: Event, disk_full: bool) -> io::Error {
    if disk_full {
        #[cfg(windows)]
        let code = 112;
        #[cfg(not(windows))]
        let code = 28;
        io::Error::from_raw_os_error(code)
    } else {
        io::Error::other(format!("injected {event:?} I/O failure"))
    }
}
pub(crate) fn observe(event: Event) {
    point(event).expect("only pause actions belong on observation checkpoints");
}

struct Probe {
    _scope: Scope,
    injection: Arc<Injection>,
    reached: Option<mpsc::Receiver<()>>,
    resume: Option<mpsc::Sender<()>>,
}
struct Resume(mpsc::Sender<()>);
impl Drop for Resume {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
impl Probe {
    fn pause(event: Event) -> Self {
        Self::pause_after(event, 0, None)
    }
    fn pause_after(event: Event, skip: usize, failure: Option<bool>) -> Self {
        let (entered, reached) = mpsc::channel();
        let (resume, receiver) = mpsc::channel();
        let injection = Arc::new(Injection {
            event,
            skip,
            seen: AtomicUsize::new(0),
            fired: AtomicBool::new(false),
            action: Action::Pause {
                reached: Mutex::new(Some(entered)),
                resume: Mutex::new(receiver),
                failure,
            },
        });
        Self {
            _scope: install(Some(Arc::clone(&injection))),
            injection,
            reached: Some(reached),
            resume: Some(resume),
        }
    }
    fn fail(event: Event, disk_full: bool) -> Self {
        let injection = Arc::new(Injection {
            event,
            skip: 0,
            seen: AtomicUsize::new(0),
            fired: AtomicBool::new(false),
            action: Action::Fail { disk_full },
        });
        Self {
            _scope: install(Some(Arc::clone(&injection))),
            injection,
            reached: None,
            resume: None,
        }
    }
    fn wait(&self) -> Resume {
        self.reached
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!(
                    "checkpoint {:?} was not reached: {error}",
                    self.injection.event
                )
            });
        Resume(self.resume.as_ref().unwrap().clone())
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(());
        }
    }
}

const MODES: [JoinMode; 4] = [
    JoinMode::Left,
    JoinMode::Inner,
    JoinMode::Right,
    JoinMode::Full,
];
const MAIN: &[u8] =
    b"key,region,label\nk,r,\"Main, one\"\nk,r,Main-two\nz,u,Unmatched\nk,,Empty-key\n";
const ADDITIONAL: &[u8] =
    b"key\tregion\tcity\nk\tr\tS\xe3o Paulo\nk\tr\tMontr\xe9al\nq\tv\tExtra\n";
const FULL_RESULT: &str = "label,city\n\"Main, one\",São Paulo\n\"Main, one\",Montréal\nMain-two,São Paulo\nMain-two,Montréal\nUnmatched,\nEmpty-key,\n,Extra\n";

struct Fixture {
    directory: tempfile::TempDir,
    paths: [PathBuf; 2],
    output: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = [
            directory.path().join("main.csv"),
            directory.path().join("additional.tsv"),
        ];
        std::fs::write(&paths[0], MAIN).unwrap();
        std::fs::write(&paths[1], ADDITIONAL).unwrap();
        std::fs::write(
            directory.path().join("unrelated.csv"),
            b"preserve existing\n",
        )
        .unwrap();
        let output = directory.path().join("joined.csv");
        Self {
            directory,
            paths,
            output,
        }
    }
    fn analyze(&self, mode: JoinMode) -> JoinJob<JoinPlan> {
        let inputs = std::array::from_fn(|side| JoinInput {
            path: self.paths[side].clone(),
            delimiter: Some(if side == 0 { b',' } else { b'\t' }),
            has_header: true,
            encoding: Some(if side == 0 {
                Encoding::Utf8
            } else {
                Encoding::Windows1252
            }),
            key_columns: vec![0, 1],
            expected_version: None,
            require_unique: false,
        });
        let config = JoinConfig {
            mode,
            case: JoinCase::Sensitive,
            memory_bytes: 2 * 1024 * 1024,
            run_memory_bytes: 128,
            group_memory_bytes: 16,
            max_record_bytes: 1024,
            max_columns: 16,
            max_merge_fan_in: 2,
            preview_rows: 2,
            preview_bytes: 512,
        };
        let output = JoinOutputOptions {
            columns: Some(vec![
                JoinOutputColumn {
                    source: JoinSource::Main,
                    column: 2,
                    name: Some("label".into()),
                },
                JoinOutputColumn {
                    source: JoinSource::Additional,
                    column: 2,
                    name: Some("city".into()),
                },
            ]),
            include_header: Some(true),
            ..Default::default()
        };
        analyze_join_with_output(inputs, self.directory.path().to_owned(), config, output).unwrap()
    }
    fn plan(&self, mode: JoinMode) -> JoinPlan {
        self.analyze(mode).wait().unwrap().unwrap()
    }
    fn workspaces(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".quarry-")
            })
            .collect()
    }
    fn assert_inputs(&self) {
        assert_eq!(std::fs::read(&self.paths[0]).unwrap(), MAIN);
        assert_eq!(std::fs::read(&self.paths[1]).unwrap(), ADDITIONAL);
        assert_eq!(
            std::fs::read(self.directory.path().join("unrelated.csv")).unwrap(),
            b"preserve existing\n"
        );
    }
    fn assert_unpublished_and_clean(&self) {
        assert!(!self.output.exists());
        assert!(
            self.workspaces().is_empty(),
            "private data remained: {:?}",
            self.workspaces()
        );
        self.assert_inputs();
    }
}

#[test]
fn cancellation_at_every_analysis_phase_releases_private_data_in_all_modes() {
    for mode in MODES {
        for event in [
            Event::Phase(JoinPhase::Inspecting),
            Event::Phase(JoinPhase::Sorting),
            Event::Merge,
            Event::Phase(JoinPhase::Analyzing),
        ] {
            let fixture = Fixture::new();
            let probe = Probe::pause(event);
            let job = fixture.analyze(mode);
            let resume = probe.wait();
            assert!(!job.progress().done);
            job.cancel();
            drop(resume);
            assert!(job.wait().unwrap().is_none(), "{mode:?} {event:?}");
            fixture.assert_unpublished_and_clean();
        }
    }
}

#[test]
fn cancellation_through_expansion_ordering_writing_and_sync_never_publishes() {
    for mode in MODES {
        for event in [
            Event::Phase(JoinPhase::Creating),
            Event::ExpandingPairs,
            Event::RestoringOrder,
            Event::WritingRow,
            Event::Phase(JoinPhase::Publishing),
            Event::OutputFlush,
            Event::OutputSync,
        ] {
            let fixture = Fixture::new();
            let plan = fixture.plan(mode);
            let probe = Probe::pause(event);
            let job = plan
                .start_write_acknowledged(fixture.output.clone(), true)
                .unwrap();
            let resume = probe.wait();
            assert!(!job.progress().done);
            job.cancel();
            drop(resume);
            assert!(job.wait().unwrap().is_none(), "{mode:?} {event:?}");
            fixture.assert_unpublished_and_clean();
        }
    }
}

#[test]
fn cancellation_after_final_source_checks_still_prevents_atomic_publication() {
    for mode in MODES {
        let fixture = Fixture::new();
        let plan = fixture.plan(mode);
        let probe = Probe::pause(Event::BeforePublication);
        let job = plan
            .start_write_acknowledged(fixture.output.clone(), true)
            .unwrap();
        let resume = probe.wait();
        assert!(!fixture.output.exists());
        job.cancel();
        drop(resume);
        assert!(
            job.wait().unwrap().is_none(),
            "cancel before publication: {mode:?}"
        );
        fixture.assert_unpublished_and_clean();
    }
}

#[test]
fn cancellation_after_publication_retains_the_complete_result() {
    let fixture = Fixture::new();
    let plan = fixture.plan(JoinMode::Full);
    let expected_bytes = plan.analysis().output_bytes;
    let probe = Probe::pause(Event::AfterPublication);
    let job = plan
        .start_write_acknowledged(fixture.output.clone(), true)
        .unwrap();
    let resume = probe.wait();
    assert_eq!(
        std::fs::read(&fixture.output).unwrap(),
        FULL_RESULT.as_bytes()
    );
    job.cancel();
    drop(resume);
    let summary = job
        .wait()
        .unwrap()
        .expect("a committed result must report completion");
    assert_eq!(summary.data_rows, 7);
    assert_eq!(summary.bytes_written, expected_bytes);
    assert_eq!(
        std::fs::read(&summary.destination).unwrap(),
        FULL_RESULT.as_bytes()
    );
    assert!(fixture.workspaces().is_empty());
    fixture.assert_inputs();
}

#[test]
fn output_write_flush_sync_and_disk_full_failures_preserve_inputs_and_destinations() {
    for mode in MODES {
        for event in [Event::OutputWrite, Event::OutputFlush, Event::OutputSync] {
            for disk_full in [false, true] {
                let fixture = Fixture::new();
                let plan = fixture.plan(mode);
                let probe = Probe::fail(event, disk_full);
                let result = plan
                    .start_write_acknowledged(fixture.output.clone(), true)
                    .unwrap()
                    .wait();
                assert!(probe.injection.fired.load(Ordering::Acquire));
                let error = result.unwrap_err();
                let phase = if event == Event::OutputWrite {
                    JoinPhase::Creating
                } else {
                    JoinPhase::Publishing
                };
                assert!(
                    error.to_string().contains(phase.label())
                        && error.to_string().contains("analyze again"),
                    "output failure must explain its phase and recovery: {error}"
                );
                assert!(
                    matches!(&error, QuarryError::Storage { directory, .. }
                    if directory.canonicalize().unwrap() == fixture.directory.path().canonicalize().unwrap()),
                    "{event:?}: {error:?}"
                );
                fixture.assert_unpublished_and_clean();
            }
        }
    }
}

#[test]
fn a_destination_created_at_the_last_checkpoint_is_never_overwritten() {
    let fixture = Fixture::new();
    let plan = fixture.plan(JoinMode::Full);
    let probe = Probe::pause(Event::BeforePublication);
    let job = plan
        .start_write_acknowledged(fixture.output.clone(), true)
        .unwrap();
    let resume = probe.wait();
    std::fs::write(&fixture.output, b"another writer won\n").unwrap();
    drop(resume);
    assert!(matches!(
        job.wait(),
        Err(QuarryError::ExportDestinationExists)
    ));
    assert_eq!(
        std::fs::read(&fixture.output).unwrap(),
        b"another writer won\n"
    );
    assert!(fixture.workspaces().is_empty());
    fixture.assert_inputs();
}

#[test]
fn final_source_changes_are_rechecked_before_publication() {
    let fixture = Fixture::new();
    let plan = fixture.plan(JoinMode::Full);
    let probe = Probe::pause(Event::BeforeSourceGuard);
    let job = plan
        .start_write_acknowledged(fixture.output.clone(), true)
        .unwrap();
    let resume = probe.wait();
    std::fs::write(&fixture.paths[0], b"new external contents\n").unwrap();
    drop(resume);
    let outcome = job.wait();
    assert!(
        matches!(&outcome, Err(QuarryError::JoinFile { path: Some(path), reason, .. })
        if path == &fixture.paths[0] && reason.contains("Main") && reason.contains("analyze again")),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read(&fixture.paths[0]).unwrap(),
        b"new external contents\n"
    );
    assert_eq!(std::fs::read(&fixture.paths[1]).unwrap(), ADDITIONAL);
    assert!(!fixture.output.exists());
    assert!(fixture.workspaces().is_empty());
}

#[test]
fn cleanup_failure_keeps_both_cancellation_and_preserved_foreign_data_visible() {
    let fixture = Fixture::new();
    let probe = Probe::pause(Event::Phase(JoinPhase::Analyzing));
    let job = fixture.analyze(JoinMode::Full);
    let resume = probe.wait();
    let roots = fixture.workspaces();
    assert_eq!(roots.len(), 1);
    let foreign = roots[0].join("foreign.csv");
    std::fs::write(&foreign, b"preserve foreign\n").unwrap();
    job.cancel();
    drop(resume);
    let error = job
        .wait()
        .err()
        .expect("cleanup failure must not disappear into cancellation");
    let message = error.to_string();
    assert!(
        message.contains("cancelled") && message.contains("Could not remove private join data"),
        "{message}"
    );
    assert_eq!(std::fs::read(foreign).unwrap(), b"preserve foreign\n");
    assert_eq!(
        std::fs::read_dir(&roots[0]).unwrap().count(),
        1,
        "remaining: {:?}",
        std::fs::read_dir(&roots[0])
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    assert!(!fixture.output.exists());
    fixture.assert_inputs();
}

#[test]
fn successful_publication_retains_result_even_when_private_cleanup_fails() {
    let fixture = Fixture::new();
    let plan = fixture.plan(JoinMode::Full);
    let probe = Probe::pause(Event::AfterPublication);
    let job = plan
        .start_write_acknowledged(fixture.output.clone(), true)
        .unwrap();
    let resume = probe.wait();
    let roots: Vec<_> = fixture
        .workspaces()
        .into_iter()
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".quarry-join-")
        })
        .collect();
    assert_eq!(roots.len(), 1);
    let foreign = roots[0].join("foreign.csv");
    std::fs::write(&foreign, b"preserve foreign\n").unwrap();
    job.cancel();
    drop(resume);
    let summary = job
        .wait()
        .unwrap()
        .expect("published output must stay completed");
    assert!(summary.cleanup_warning.is_some());
    assert_eq!(
        std::fs::read(&summary.destination).unwrap(),
        FULL_RESULT.as_bytes()
    );
    assert_eq!(std::fs::read(foreign).unwrap(), b"preserve foreign\n");
    assert_eq!(
        std::fs::read_dir(&roots[0]).unwrap().count(),
        1,
        "remaining: {:?}",
        std::fs::read_dir(&roots[0])
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    fixture.assert_inputs();
}

#[test]
fn completed_result_survives_foreign_files_in_output_staging() {
    let fixture = Fixture::new();
    let plan = fixture.plan(JoinMode::Full);
    let probe = Probe::pause(Event::AfterPublication);
    let job = plan
        .start_write_acknowledged(fixture.output.clone(), true)
        .unwrap();
    let resume = probe.wait();
    let roots = fixture.workspaces();
    assert_eq!(
        roots.len(),
        2,
        "both scratch and output staging are live here"
    );
    let staging = roots
        .iter()
        .find(|path| {
            !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".quarry-join-")
        })
        .unwrap();
    let foreign = staging.join("foreign.csv");
    std::fs::write(&foreign, b"preserve staging foreign\n").unwrap();
    job.cancel();
    drop(resume);
    let summary = job
        .wait()
        .unwrap()
        .expect("publication must remain completed");
    assert!(
        summary
            .cleanup_warning
            .as_ref()
            .is_some_and(|warning| warning.contains("private output staging"))
    );
    assert_eq!(
        std::fs::read(summary.destination).unwrap(),
        FULL_RESULT.as_bytes()
    );
    assert_eq!(
        std::fs::read(&foreign).unwrap(),
        b"preserve staging foreign\n"
    );
    assert_eq!(std::fs::read_dir(staging).unwrap().count(), 1);
    assert_eq!(fixture.workspaces(), vec![staging.to_owned()]);
    fixture.assert_inputs();
}

#[test]
fn private_spill_io_failures_report_the_phase_folder_and_recovery() {
    for mode in MODES {
        for writing in [false, true] {
            for event in [Event::SpillWrite, Event::SpillFlush] {
                for disk_full in [false, true] {
                    let fixture = Fixture::new();
                    let plan = writing.then(|| fixture.plan(mode));
                    let probe = Probe::fail(event, disk_full);
                    let error = if let Some(plan) = plan {
                        plan.start_write_acknowledged(fixture.output.clone(), true)
                            .unwrap()
                            .wait()
                            .expect_err("expected private output-ordering I/O failure")
                    } else {
                        fixture
                            .analyze(mode)
                            .wait()
                            .err()
                            .expect("expected private input-preparation I/O failure")
                    };
                    assert!(probe.injection.fired.load(Ordering::Acquire));
                    let phase = if writing {
                        JoinPhase::Creating
                    } else {
                        JoinPhase::Sorting
                    };
                    assert!(
                        matches!(&error, QuarryError::Storage { directory, error }
                        if directory.parent() == Some(fixture.directory.path())
                            && error.to_string().contains(phase.label())
                            && error.to_string().contains("analyze again")),
                        "{writing} {event:?}: {error:?}"
                    );
                    fixture.assert_unpublished_and_clean();
                }
            }
        }
    }
}

#[test]
fn disk_full_after_real_staging_writes_removes_partial_private_output() {
    use std::io::Read;

    let directory = tempfile::tempdir().unwrap();
    let paths = [
        directory.path().join("main.csv"),
        directory.path().join("additional.csv"),
    ];
    let sources: [Vec<u8>; 2] = std::array::from_fn(|side| {
        let mut bytes = b"key,value\n".to_vec();
        for row in 0..if side == 0 { 32 } else { 33 } {
            bytes.extend_from_slice(
                format!("k,{}-{row}-{}\n", side, "payload".repeat(72)).as_bytes(),
            );
        }
        bytes
    });
    for side in 0..2 {
        std::fs::write(&paths[side], &sources[side]).unwrap();
    }
    let inputs = std::array::from_fn(|side| JoinInput {
        path: paths[side].clone(),
        delimiter: Some(b','),
        has_header: true,
        encoding: Some(Encoding::Utf8),
        key_columns: vec![0],
        expected_version: None,
        require_unique: false,
    });
    let config = JoinConfig {
        memory_bytes: 2 * 1024 * 1024,
        max_record_bytes: 4096,
        run_memory_bytes: 1024,
        group_memory_bytes: 128,
        max_columns: 8,
        max_merge_fan_in: 4,
        preview_rows: 0,
        preview_bytes: 0,
        ..Default::default()
    };
    let plan = analyze_join(inputs, directory.path().to_owned(), config)
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
    assert_eq!(plan.analysis().matching_pairs, 32 * 33);
    let output = directory.path().join("joined.csv");
    let probe = Probe::pause_after(Event::OutputWrite, 64, Some(true));
    let job = plan.start_write_acknowledged(output.clone(), true).unwrap();
    let resume = probe.wait();
    let mut staged = None;
    for root in std::fs::read_dir(directory.path()).unwrap() {
        let root = root.unwrap().path();
        if !root.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&root).unwrap() {
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_file() {
                continue;
            }
            // Measure the opened staging file, independently of cached directory metadata.
            let mut staged_file = std::fs::File::open(entry.path()).unwrap();
            if staged_file.metadata().unwrap().len() < 8192 {
                continue;
            }
            let mut prefix = [0_u8; 10];
            staged_file.read_exact(&mut prefix).unwrap();
            if &prefix == b"key (Main)" {
                staged = Some((root.clone(), entry.path()));
            }
        }
    }
    let (root, file) = staged.expect("partial CSV data must have reached real private staging");
    assert!(
        std::fs::File::open(&file)
            .unwrap()
            .metadata()
            .unwrap()
            .len()
            >= 8192
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(!output.exists());
    drop(resume);
    assert!(matches!(job.wait(), Err(QuarryError::Storage { .. })));
    assert!(!output.exists());
    assert!(!root.exists());
    for side in 0..2 {
        assert_eq!(std::fs::read(&paths[side]).unwrap(), sources[side]);
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn original_write_failure_is_retained_when_output_staging_cleanup_also_fails() {
    for event in [Event::OutputWrite, Event::OutputFlush, Event::OutputSync] {
        let fixture = Fixture::new();
        let plan = fixture.plan(JoinMode::Full);
        let probe = Probe::pause_after(event, 0, Some(false));
        let job = plan
            .start_write_acknowledged(fixture.output.clone(), true)
            .unwrap();
        let resume = probe.wait();
        let roots = fixture.workspaces();
        let staging = roots
            .iter()
            .find(|path| {
                !path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".quarry-join-")
            })
            .unwrap();
        let foreign = staging.join("foreign.csv");
        std::fs::write(&foreign, b"preserve staging foreign\n").unwrap();
        drop(resume);
        let error = job
            .wait()
            .expect_err("both write and cleanup failures must be reported");
        let reason = error.to_string();
        assert!(
            reason.contains(&format!("injected {event:?} I/O failure"))
                && reason.contains("Could not remove output staging"),
            "{reason}"
        );
        assert!(!fixture.output.exists());
        assert_eq!(
            std::fs::read(&foreign).unwrap(),
            b"preserve staging foreign\n"
        );
        assert_eq!(std::fs::read_dir(staging).unwrap().count(), 1);
        assert_eq!(fixture.workspaces(), vec![staging.to_owned()]);
        fixture.assert_inputs();
    }
}

#[test]
fn cancellation_and_source_conflict_remain_visible_when_staging_cleanup_fails() {
    for changed_source in [false, true] {
        let fixture = Fixture::new();
        let plan = fixture.plan(JoinMode::Full);
        let probe = Probe::pause(if changed_source {
            Event::BeforeSourceGuard
        } else {
            Event::BeforePublication
        });
        let job = plan
            .start_write_acknowledged(fixture.output.clone(), true)
            .unwrap();
        let resume = probe.wait();
        let roots = fixture.workspaces();
        let staging = roots
            .iter()
            .find(|path| {
                !path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".quarry-join-")
            })
            .unwrap();
        let foreign = staging.join("foreign.csv");
        std::fs::write(&foreign, b"preserve staging foreign\n").unwrap();
        if changed_source {
            std::fs::write(&fixture.paths[0], b"external update\n").unwrap();
        } else {
            job.cancel();
        }
        drop(resume);
        let error = job.wait().expect_err("cleanup failure must remain visible");
        let message = error.to_string();
        assert!(
            message.contains("Could not remove output staging"),
            "{message}"
        );
        if changed_source {
            assert!(message.contains("Main input changed"), "{message}");
            assert_eq!(
                std::fs::read(&fixture.paths[0]).unwrap(),
                b"external update\n"
            );
        } else {
            assert!(message.contains("cancelled"), "{message}");
            fixture.assert_inputs();
        }
        assert!(!fixture.output.exists());
        assert_eq!(
            std::fs::read(&foreign).unwrap(),
            b"preserve staging foreign\n"
        );
        assert_eq!(std::fs::read_dir(staging).unwrap().count(), 1);
        assert_eq!(fixture.workspaces(), vec![staging.to_owned()]);
        assert_eq!(std::fs::read(&fixture.paths[1]).unwrap(), ADDITIONAL);
    }
}

#[cfg(unix)]
#[test]
fn creation_capacity_follows_pinned_folders_after_rename_and_replacement() {
    for target in ["temporary", "output"] {
        for replacement in ["missing", "directory", "file"] {
            let fixture = Fixture::new();
            let output = fixture.directory.path().join("output");
            std::fs::create_dir(&output).unwrap();
            let plan = fixture.plan(JoinMode::Full);
            let workspace = fixture.workspaces().pop().unwrap();
            let original = if target == "temporary" {
                &workspace
            } else {
                &output
            };
            let moved = fixture.directory.path().join("moved");
            let probe = Probe::pause(Event::DestinationPinned);
            let job = plan
                .start_write_acknowledged(output.join("joined.csv"), true)
                .unwrap();
            let resume = probe.wait();
            std::fs::rename(original, &moved).unwrap();
            match replacement {
                "directory" => {
                    std::fs::create_dir(original).unwrap();
                    std::fs::write(original.join("foreign.csv"), b"preserve replacement\n")
                        .unwrap();
                }
                "file" => std::fs::write(original, b"preserve replacement\n").unwrap(),
                _ => {}
            }
            drop(resume);
            let summary = job
                .wait()
                .unwrap_or_else(|error| panic!("{target} {replacement}: {error}"))
                .unwrap();
            let result = if target == "output" { &moved } else { &output }.join("joined.csv");
            assert_eq!(
                summary.destination.canonicalize().unwrap(),
                result.canonicalize().unwrap()
            );
            assert_eq!(summary.data_rows, 7);
            assert_eq!(summary.bytes_written, FULL_RESULT.len() as u64);
            assert_eq!(std::fs::read(&result).unwrap(), FULL_RESULT.as_bytes());
            assert_eq!(
                std::fs::read_dir(result.parent().unwrap()).unwrap().count(),
                1
            );
            assert!(summary.cleanup_warning.is_none());
            assert!(
                !(if target == "temporary" {
                    &moved
                } else {
                    &workspace
                })
                .exists()
            );
            match replacement {
                "directory" => {
                    assert_eq!(
                        std::fs::read(original.join("foreign.csv")).unwrap(),
                        b"preserve replacement\n"
                    );
                    assert_eq!(std::fs::read_dir(original).unwrap().count(), 1);
                }
                "file" => assert_eq!(std::fs::read(original).unwrap(), b"preserve replacement\n"),
                _ => assert!(!original.exists()),
            }
            fixture.assert_inputs();
        }
    }
}
