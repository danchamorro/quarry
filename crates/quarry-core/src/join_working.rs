use super::*;
use std::collections::BTreeMap;

/// Immutable references to already-owned document edit metadata and values.
pub type JoinHeaderEdits = Arc<BTreeMap<usize, Arc<String>>>;
pub type JoinCellEdits = Arc<BTreeMap<(u64, usize), Arc<Vec<u8>>>>;
/// The caller's lease owns private generations and releases its reservation on Drop.
pub type JoinInputOwner = Arc<dyn Send + Sync>;

/// A constant-size capture: no edit map, bootstrap rows or field payload is copied.
#[derive(Clone)]
pub struct JoinWorkingSource {
    pub(super) session: Arc<crate::Session>,
    logical_path: PathBuf,
    logical_stamp: SourceStamp,
    headers: JoinHeaderEdits,
    cells: JoinCellEdits,
    // Drop the directory pin before the final generation owner on Windows.
    directory: Option<Arc<crate::destination::Directory>>,
    _owner: JoinInputOwner,
}

impl std::fmt::Debug for JoinWorkingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinWorkingSource")
            .field("logical_path", &self.logical_path)
            .finish_non_exhaustive()
    }
}

impl JoinWorkingSource {
    /// Capture current physical values and the saved source they derive from.
    /// The owner must retain every private generation used by the current session.
    /// Callers must commit valid drafts and reserve their document before capture.
    pub fn capture(
        session: Arc<crate::Session>,
        logical_source: &crate::Session,
        headers: JoinHeaderEdits,
        cells: JoinCellEdits,
        owner: JoinInputOwner,
    ) -> Self {
        Self {
            session,
            logical_path: logical_source.path.clone(),
            logical_stamp: logical_source.source_stamp.clone(),
            headers,
            cells,
            directory: None,
            _owner: owner,
        }
    }

    pub fn logical_path(&self) -> &Path {
        &self.logical_path
    }

    pub fn format(&self) -> JoinFormat {
        JoinFormat {
            delimiter: Some(self.session.dialect.delimiter),
            has_header: self.session.dialect.has_header,
            encoding: Some(self.session.encoding),
        }
    }

    #[cfg(not(unix))]
    pub(super) fn logical_stamp(&self) -> &SourceStamp {
        &self.logical_stamp
    }

    pub(super) fn guards(&self) -> [(PathBuf, SourceStamp); 2] {
        [
            (self.session.path.clone(), self.session.source_stamp.clone()),
            (self.logical_path.clone(), self.logical_stamp.clone()),
        ]
    }

    /// Capture private generation membership on the worker, never on the UI.
    pub(super) fn pin_private_directory(&mut self) -> JResult<()> {
        // Even a hard link can live in an owned generation removed by its lease.
        if self.session.path != self.logical_path {
            let parent = output_parent(&self.session.path);
            let directory = crate::destination::Directory::open(parent)
                .map_err(|error| input_error(&self.logical_path, None, error))?;
            self.validate()?;
            if !directory.still_named_by(parent) {
                return Err(input_error(
                    &self.logical_path,
                    None,
                    "The current working directory changed. Analyze again.",
                )
                .into());
            }
            self.directory = Some(Arc::new(directory));
        }
        Ok(())
    }

    pub(super) fn ensure_output_outside(
        &self,
        destination: &crate::destination::PreparedDestination,
    ) -> Result<(), QuarryError> {
        if let Some(directory) = &self.directory {
            destination.ensure_outside_directory(directory)?;
        }
        Ok(())
    }

    pub(super) fn validate(&self) -> JResult<()> {
        if !self.session.dialect.has_header && !self.headers.is_empty() {
            return Err(input_error(
                &self.logical_path,
                None,
                "Header changes cannot be applied to a document without a header.",
            )
            .into());
        }
        for (path, stamp) in self.guards() {
            let file = open_regular_file(&path)
                .map_err(|error| input_error(&self.logical_path, None, error))?;
            if !source_matches_stamp(&file, &path, &stamp)? {
                return Err(input_error(
                    &self.logical_path,
                    None,
                    "The captured document's source changed. Reopen it and analyze again.",
                )
                .into());
            }
        }
        Ok(())
    }

    pub(super) fn accepts(&self, input: &JoinInput) -> JResult<()> {
        self.validate()?;
        let selected = open_regular_file(&input.path)
            .map_err(|error| input_error(&input.path, None, error))?;
        let selected_stamp = SourceStamp::from_file(&selected)?;
        if !self.session.source_stamp.matches(&selected_stamp)
            && !self.logical_stamp.matches(&selected_stamp)
        {
            return Err(input_error(
                &input.path,
                None,
                "The selected path does not belong to the captured document.",
            )
            .into());
        }
        let format = self.format();
        if input.has_header != format.has_header
            || input
                .delimiter
                .is_some_and(|value| Some(value) != format.delimiter)
            || input
                .encoding
                .is_some_and(|value| Some(value) != format.encoding)
        {
            return Err(input_error(
                &self.logical_path,
                None,
                "Use the open document's applied delimiter, header and encoding settings.",
            )
            .into());
        }
        Ok(())
    }

    pub(super) fn check_rows(&self, rows: u64) -> JResult<()> {
        if self
            .cells
            .last_key_value()
            .is_some_and(|((row, _), _)| *row >= rows)
        {
            return Err(input_error(
                &self.logical_path,
                None,
                "A captured edit refers to a row outside the current document.",
            )
            .into());
        }
        Ok(())
    }

    /// Overlay only the current row. Edited values borrow existing immutable data;
    /// encoding allocates at most one capped record and never a file-sized copy.
    pub(super) fn record<'a>(
        &self,
        record: &'a [u8],
        row: u64,
        config: JoinConfig,
        state: &Progress,
    ) -> JResult<Cow<'a, [u8]>> {
        let header = self.session.dialect.has_header && row == 0;
        let edits = self.cells.range((row, 0)..=(row, usize::MAX));
        if header && edits.clone().next().is_some() {
            return Err(input_error(
                &self.logical_path,
                Some(1),
                "A captured data-cell edit refers to the header row.",
            )
            .into());
        }
        if if header {
            self.headers.is_empty()
        } else {
            edits.clone().next().is_none()
        } {
            return Ok(Cow::Borrowed(record));
        }
        // Parse the structure first, then validate the effective current fields.
        // A valid edit may repair an invalid UTF-8 field in the saved bytes.
        let mut parsed = parse_record_with_field_limit(
            record,
            self.session.dialect.delimiter,
            config.max_columns,
        )
        .map_err(|error| input_error(&self.logical_path, Some(row + 1), error))?;
        if header {
            for (&column, value) in self.headers.iter() {
                state.check()?;
                if value.len() > config.max_record_bytes {
                    return Err(input_error(
                        &self.logical_path,
                        Some(1),
                        "A captured header exceeds the record size limit.",
                    )
                    .into());
                }
                let field = parsed.get_mut(column).ok_or_else(|| {
                    input_error(
                        &self.logical_path,
                        Some(1),
                        "A captured header refers to a missing column.",
                    )
                })?;
                *field = Cow::Borrowed(value.as_bytes());
            }
        } else {
            for (&(_, column), value) in edits {
                state.check()?;
                if value.len() > config.max_record_bytes || std::str::from_utf8(value).is_err() {
                    return Err(input_error(
                        &self.logical_path,
                        Some(row + 1),
                        "A captured cell exceeds the record limit or is not valid UTF-8.",
                    )
                    .into());
                }
                let field = parsed.get_mut(column).ok_or_else(|| {
                    input_error(
                        &self.logical_path,
                        Some(row + 1),
                        "A captured edit refers to a missing field.",
                    )
                })?;
                *field = Cow::Borrowed(value.as_slice());
            }
        }
        if parsed
            .iter()
            .any(|field| std::str::from_utf8(field).is_err())
        {
            return Err(input_error(
                &self.logical_path,
                Some(row + 1),
                "Invalid UTF-8 text. Choose the correct encoding before joining.",
            )
            .into());
        }
        encode(
            parsed.iter().map(|field| field.as_ref()),
            self.session.dialect.delimiter,
            config.max_record_bytes,
        )
        .map(Cow::Owned)
        .map_err(|failure| match failure {
            Failure::Error(error) => input_error(&self.logical_path, Some(row + 1), error).into(),
            cancelled => cancelled,
        })
    }
}

/// Read bounded picker metadata off-thread, using the document's applied format.
pub fn read_join_working_header(
    source: JoinWorkingSource,
    known_columns: usize,
) -> Result<JoinJob<JoinHeader>, QuarryError> {
    if known_columns > COLUMN_LIMIT {
        return Err(error("The current document exceeds the join column limit."));
    }
    JoinJob::spawn(move |state| {
        state.check()?;
        source.validate()?;
        let mut header =
            header_info_using(&source.session.path, source.format(), Some(&source), state)
                .map_err(|failure| match failure {
                    Failure::Error(QuarryError::JoinFile { record, reason, .. }) => {
                        input_error(&source.logical_path, record, reason).into()
                    }
                    Failure::Error(error) => input_error(&source.logical_path, None, error).into(),
                    cancelled => cancelled,
                })?;
        header
            .columns
            .resize(header.columns.len().max(known_columns), String::new());
        let each = (512 * 1024 / header.columns.len().max(1)).min(256);
        for name in &mut header.columns {
            *name = clip_label(name, each);
        }
        source.validate()?;
        Ok(header)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HeaderMode, OpenOptions, Session};

    fn session(
        path: &Path,
        delimiter: u8,
        header: bool,
        encoding: Option<Encoding>,
    ) -> Arc<Session> {
        Arc::new(
            Session::open_with_encoding(
                path,
                OpenOptions {
                    delimiter: Some(delimiter),
                    header_mode: if header {
                        HeaderMode::FirstRow
                    } else {
                        HeaderMode::NoHeader
                    },
                    ..OpenOptions::default()
                },
                encoding,
            )
            .unwrap(),
        )
    }

    fn input(path: &Path, key: usize, header: bool) -> JoinInput {
        JoinInput {
            path: path.to_owned(),
            delimiter: None,
            has_header: header,
            encoding: None,
            key_columns: vec![key],
            expected_version: None,
            require_unique: false,
        }
    }

    fn config() -> JoinConfig {
        JoinConfig {
            memory_bytes: 1024 * 1024,
            run_memory_bytes: 128,
            group_memory_bytes: 128,
            max_record_bytes: 1024,
            max_columns: 32,
            max_merge_fan_in: 2,
            preview_rows: 8,
            preview_bytes: 4096,
            ..JoinConfig::default()
        }
    }

    fn analyze(
        inputs: [JoinInput; 2],
        working: [Option<JoinWorkingSource>; 2],
        temporary: &Path,
    ) -> Result<Option<JoinPlan>, QuarryError> {
        analyze_join_with_working(
            inputs,
            working,
            temporary.to_owned(),
            config(),
            JoinOutputOptions::default(),
        )?
        .wait()
    }

    struct Owner {
        _directory: Arc<tempfile::TempDir>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[test]
    fn frozen_working_values_retain_the_generation_and_preserve_both_sources() {
        for action in ["release", "drop", "create"] {
            let parent = tempfile::tempdir().unwrap();
            let logical = parent.path().join("main.csv");
            let additional = parent.path().join("additional.csv");
            fs::write(&logical, b"id,name\nold,Original\n").unwrap();
            fs::write(&additional, b"id,city\nnew-key,Paris\nold,Wrong\n").unwrap();
            let original = session(&logical, b',', true, None);
            let directory = Arc::new(tempfile::tempdir_in(parent.path()).unwrap());
            let generation = directory.path().to_path_buf();
            let physical = generation.join("working.csv");
            // A current structural generation has already reordered columns/rows.
            let physical_bytes = b"name,id,flag\nphysical,new-key,base\n";
            fs::write(&physical, physical_bytes).unwrap();
            let current = session(&physical, b',', true, None);
            let headers = Arc::new(BTreeMap::from([(0, Arc::new("Customer".to_owned()))]));
            let cells = Arc::new(BTreeMap::from([
                ((1, 0), Arc::new("Renamed,\n\"É\"".as_bytes().to_vec())),
                ((1, 2), Arc::new(b"changed".to_vec())),
            ]));
            let dropped = Arc::new(AtomicBool::new(false));
            let owner = Arc::new(Owner {
                _directory: Arc::clone(&directory),
                dropped: Arc::clone(&dropped),
            });
            let source = JoinWorkingSource::capture(
                current,
                &original,
                Arc::clone(&headers),
                Arc::clone(&cells),
                owner,
            );
            assert!(
                Arc::ptr_eq(&source.headers, &headers),
                "capture must share the edit map"
            );
            assert!(
                Arc::ptr_eq(&source.cells, &cells),
                "capture must share the edit map"
            );
            let header = read_join_working_header(source.clone(), 3)
                .unwrap()
                .wait()
                .unwrap()
                .unwrap();
            assert_eq!(header.columns, ["Customer", "id", "flag"]);
            let mut main_input = input(&logical, 1, true);
            main_input.expected_version = Some(header.source_version);
            drop(headers);
            drop(cells);
            drop(original);
            drop(directory);
            let plan = analyze(
                [main_input, input(&additional, 0, true)],
                [Some(source), None],
                parent.path(),
            )
            .unwrap()
            .unwrap();
            assert!(plan.inputs()[0].working_values);
            assert!(!plan.inputs()[1].working_values);
            assert_eq!(plan.inputs()[0].path, logical);
            assert!(!dropped.load(Ordering::Acquire));
            assert!(physical.exists());
            match action {
                "release" => plan.release().unwrap(),
                "drop" => drop(plan),
                _ => {
                    let output = parent.path().join("joined.csv");
                    plan.start_write(output.clone())
                        .unwrap()
                        .wait()
                        .unwrap()
                        .unwrap();
                    assert_eq!(fs::read(output).unwrap(),
                        "Customer,id (Main),flag,id (Additional),city\n\"Renamed,\n\"\"É\"\"\",new-key,changed,new-key,Paris\n".as_bytes());
                }
            }
            assert!(dropped.load(Ordering::Acquire));
            assert!(
                !generation.exists(),
                "the final lease releases the private generation"
            );
            assert_eq!(fs::read(logical).unwrap(), b"id,name\nold,Original\n");
            assert_eq!(
                fs::read(additional).unwrap(),
                b"id,city\nnew-key,Paris\nold,Wrong\n"
            );
        }
    }

    #[test]
    fn working_formats_are_inherited_and_overlay_text_remains_utf8() {
        let parent = tempfile::tempdir().unwrap();
        let main = parent.path().join("main.tsv");
        let additional = parent.path().join("additional.csv");
        fs::write(&main, b"id\tname\nx\tcaf\xe9\n").unwrap();
        fs::write(&additional, b"id,city\nx,Paris\n").unwrap();
        let current = session(&main, b'\t', true, Some(Encoding::Windows1252));
        let source = JoinWorkingSource::capture(
            Arc::clone(&current),
            &current,
            Arc::new(BTreeMap::new()),
            Arc::new(BTreeMap::from([(
                (1, 1),
                Arc::new("€".as_bytes().to_vec()),
            )])),
            Arc::new(()),
        );
        let plan = analyze(
            [input(&main, 0, true), input(&additional, 0, true)],
            [Some(source.clone()), None],
            parent.path(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.inputs()[0].delimiter, b'\t');
        assert_eq!(plan.inputs()[0].encoding, Encoding::Windows1252);
        let output = parent.path().join("joined.tsv");
        plan.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(output).unwrap(),
            "id (Main)\tname\tid (Additional)\tcity\nx\t€\tx\tParis\n".as_bytes()
        );
        assert_eq!(fs::read(&main).unwrap(), b"id\tname\nx\tcaf\xe9\n");
        let mut wrong = input(&main, 0, true);
        wrong.encoding = Some(Encoding::Utf8);
        assert!(
            analyze(
                [wrong, input(&additional, 0, true)],
                [Some(source), None],
                parent.path()
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_captured_edits_fail_with_logical_source_context() {
        let parent = tempfile::tempdir().unwrap();
        let main = parent.path().join("main.csv");
        let additional = parent.path().join("additional.csv");
        fs::write(&main, b"id,value\nx,Original\n").unwrap();
        fs::write(&additional, b"id,city\nx,Paris\n").unwrap();
        let current = session(&main, b',', true, None);
        for (row, column, bytes) in [
            (0, 1, b"header".to_vec()),
            (1, 8, b"missing".to_vec()),
            (9, 1, b"missing row".to_vec()),
            (1, 1, vec![0xff]),
            (1, 1, vec![b'x'; 1025]),
        ] {
            let source = JoinWorkingSource::capture(
                Arc::clone(&current),
                &current,
                Arc::new(BTreeMap::new()),
                Arc::new(BTreeMap::from([((row, column), Arc::new(bytes))])),
                Arc::new(()),
            );
            let error = analyze(
                [input(&main, 0, true), input(&additional, 0, true)],
                [Some(source), None],
                parent.path(),
            )
            .err()
            .expect("invalid edit must block the complete analysis");
            assert!(
                matches!(error, QuarryError::JoinFile { path: Some(ref path), .. } if path == &main),
                "{error}"
            );
        }
        assert_eq!(fs::read(main).unwrap(), b"id,value\nx,Original\n");
    }

    #[test]
    fn separate_private_generations_of_the_same_logical_source_are_not_self_joined() {
        let parent = tempfile::tempdir().unwrap();
        let logical = parent.path().join("main.csv");
        fs::write(&logical, b"id,value\nx,Original\n").unwrap();
        let original = session(&logical, b',', true, None);
        let mut sources = Vec::new();
        let mut selections = Vec::new();
        for i in 0..2 {
            let directory = Arc::new(tempfile::tempdir_in(parent.path()).unwrap());
            let path = directory.path().join("working.csv");
            fs::write(&path, format!("id,value\nx,Generation {i}\n")).unwrap();
            let source = JoinWorkingSource::capture(
                session(&path, b',', true, None),
                &original,
                Arc::new(BTreeMap::new()),
                Arc::new(BTreeMap::new()),
                directory,
            );
            selections.push(input(&path, 0, true));
            sources.push(Some(source));
        }
        assert!(
            analyze(
                selections.try_into().unwrap(),
                sources.try_into().unwrap(),
                parent.path()
            )
            .is_err()
        );
        assert_eq!(fs::read(logical).unwrap(), b"id,value\nx,Original\n");
    }

    #[test]
    fn working_source_and_generation_changes_invalidate_analysis_and_creation() {
        for which in ["logical", "physical"] {
            let parent = tempfile::tempdir().unwrap();
            let logical = parent.path().join("main.csv");
            let additional = parent.path().join("additional.csv");
            fs::write(&logical, b"id,value\nx,Original\n").unwrap();
            fs::write(&additional, b"id,city\nx,Paris\n").unwrap();
            let original = session(&logical, b',', true, None);
            let directory = Arc::new(tempfile::tempdir_in(parent.path()).unwrap());
            let physical = directory.path().join("working.csv");
            fs::write(&physical, b"id,value\nx,Current\n").unwrap();
            let source = JoinWorkingSource::capture(
                session(&physical, b',', true, None),
                &original,
                Arc::new(BTreeMap::new()),
                Arc::new(BTreeMap::new()),
                directory.clone(),
            );
            let plan = analyze(
                [input(&logical, 0, true), input(&additional, 0, true)],
                [Some(source.clone()), None],
                parent.path(),
            )
            .unwrap()
            .unwrap();
            let changed = if which == "logical" {
                &logical
            } else {
                &physical
            };
            fs::write(changed, b"id,value\nx,Changed externally\n").unwrap();
            let output = parent.path().join("joined.csv");
            assert!(plan.start_write(output.clone()).unwrap().wait().is_err());
            assert!(!output.exists());
            assert!(
                analyze(
                    [input(&logical, 0, true), input(&additional, 0, true)],
                    [Some(source), None],
                    parent.path()
                )
                .is_err()
            );
            assert_eq!(
                fs::read(changed).unwrap(),
                b"id,value\nx,Changed externally\n"
            );
        }
    }

    #[test]
    fn output_inside_a_leased_working_generation_is_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let logical = parent.path().join("main.csv");
        let additional = parent.path().join("additional.csv");
        fs::write(&logical, b"id,value\nx,Original\n").unwrap();
        fs::write(&additional, b"id,city\nx,Paris\n").unwrap();
        let original = session(&logical, b',', true, None);
        let directory = Arc::new(tempfile::tempdir_in(parent.path()).unwrap());
        let path = directory.path().join("working.csv");
        fs::write(&path, b"id,value\nx,Current\n").unwrap();
        let source = JoinWorkingSource::capture(
            session(&path, b',', true, None),
            &original,
            Arc::new(BTreeMap::new()),
            Arc::new(BTreeMap::new()),
            directory.clone(),
        );
        let plan = analyze(
            [input(&logical, 0, true), input(&additional, 0, true)],
            [Some(source), None],
            parent.path(),
        )
        .unwrap()
        .unwrap();
        let output = directory.path().join("joined.csv");
        assert!(
            plan.start_write(output.clone()).unwrap().wait().is_err(),
            "publication into an owned generation could be deleted by its last lease"
        );
        assert!(!output.exists());
        assert_eq!(fs::read(path).unwrap(), b"id,value\nx,Current\n");
    }
    #[test]
    fn both_working_inputs_match_on_current_cell_values_in_every_mode() {
        let parent = tempfile::tempdir().unwrap();
        let main = parent.path().join("main.csv");
        let additional = parent.path().join("additional.csv");
        fs::write(&main, b"id,value\nold-main,Main\n").unwrap();
        fs::write(&additional, b"id,city\nold-additional,City\n").unwrap();
        let snapshots = [
            (&main, "Main label", "Edited Main"),
            (&additional, "Additional label", "Edited Additional"),
        ]
        .map(|(path, heading, value)| {
            let current = session(path, b',', true, None);
            JoinWorkingSource::capture(
                Arc::clone(&current),
                &current,
                Arc::new(BTreeMap::from([(1, Arc::new(heading.to_owned()))])),
                Arc::new(BTreeMap::from([
                    ((1, 0), Arc::new(b"join".to_vec())),
                    ((1, 1), Arc::new(value.as_bytes().to_vec())),
                ])),
                Arc::new(()),
            )
        });
        for (index, mode) in [
            JoinMode::Left,
            JoinMode::Inner,
            JoinMode::Right,
            JoinMode::Full,
        ]
        .into_iter()
        .enumerate()
        {
            let mut cfg = config();
            cfg.mode = mode;
            let plan = analyze_join_with_working(
                [input(&main, 0, true), input(&additional, 0, true)],
                [Some(snapshots[0].clone()), Some(snapshots[1].clone())],
                parent.path().to_owned(),
                cfg,
                JoinOutputOptions::default(),
            )
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
            assert_eq!(plan.analysis().matching_pairs, 1);
            let output = parent.path().join(format!("joined-{index}.csv"));
            plan.start_write(output.clone())
                .unwrap()
                .wait()
                .unwrap()
                .unwrap();
            assert_eq!(fs::read(output).unwrap(),
                b"id (Main),Main label,id (Additional),Additional label\njoin,Edited Main,join,Edited Additional\n");
        }
        assert_eq!(fs::read(main).unwrap(), b"id,value\nold-main,Main\n");
        assert_eq!(
            fs::read(additional).unwrap(),
            b"id,city\nold-additional,City\n"
        );
    }

    #[test]
    fn current_edits_repair_utf8_while_unselected_invalid_values_still_block() {
        let parent = tempfile::tempdir().unwrap();
        let main = parent.path().join("main.csv");
        let additional = parent.path().join("additional.csv");
        fs::write(&main, b"id,value,unselected\nx,\xff,\xfe\n").unwrap();
        fs::write(&additional, b"id,city\nx,Paris\n").unwrap();
        let current = session(&main, b',', true, None);
        for replacements in 0..=2 {
            let mut cells = BTreeMap::new();
            if replacements >= 1 {
                cells.insert((1, 1), Arc::new(b"Replacement".to_vec()));
            }
            if replacements == 2 {
                cells.insert((1, 2), Arc::new(b"Fixed".to_vec()));
            }
            let source = JoinWorkingSource::capture(
                Arc::clone(&current),
                &current,
                Arc::new(BTreeMap::new()),
                Arc::new(cells),
                Arc::new(()),
            );
            let options = JoinOutputOptions {
                known_widths: Some([3, 2]),
                columns: Some(
                    [
                        (JoinSource::Main, 0),
                        (JoinSource::Main, 1),
                        (JoinSource::Additional, 0),
                        (JoinSource::Additional, 1),
                    ]
                    .map(|(source, column)| JoinOutputColumn {
                        source,
                        column,
                        name: None,
                    })
                    .to_vec(),
                ),
                include_header: Some(true),
            };
            let result = analyze_join_with_working(
                [input(&main, 0, true), input(&additional, 0, true)],
                [Some(source), None],
                parent.path().to_owned(),
                config(),
                options,
            )
            .unwrap()
            .wait();
            if replacements < 2 {
                assert!(result.is_err(), "every current field must be valid");
            } else {
                let plan = result.unwrap().unwrap();
                let output = parent.path().join("joined.csv");
                plan.start_write(output.clone())
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    fs::read(output).unwrap(),
                    b"id (Main),value,id (Additional),city\nx,Replacement,x,Paris\n"
                );
            }
        }
        assert_eq!(
            fs::read(main).unwrap(),
            b"id,value,unselected\nx,\xff,\xfe\n"
        );
    }

    #[test]
    fn current_header_names_are_used_before_text_validation_and_header_probing() {
        let parent = tempfile::tempdir().unwrap();
        let main = parent.path().join("main.csv");
        let additional = parent.path().join("additional.csv");
        fs::write(&main, b"id,\xff\nx,Current\n").unwrap();
        fs::write(&additional, b"id,city\nx,Paris\n").unwrap();
        let current = session(&main, b',', true, None);
        let source = JoinWorkingSource::capture(
            Arc::clone(&current),
            &current,
            Arc::new(BTreeMap::from([(1, Arc::new("Renamed".to_owned()))])),
            Arc::new(BTreeMap::new()),
            Arc::new(()),
        );
        let header = read_join_working_header(source.clone(), 2)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(header.columns, ["id", "Renamed"]);
        let mut main_input = input(&main, 0, true);
        main_input.expected_version = Some(header.source_version);
        let plan = analyze(
            [main_input, input(&additional, 0, true)],
            [Some(source), None],
            parent.path(),
        )
        .unwrap()
        .unwrap();
        let output = parent.path().join("joined.csv");
        plan.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(output).unwrap(),
            b"id (Main),Renamed,id (Additional),city\nx,Current,x,Paris\n"
        );
        assert_eq!(fs::read(main).unwrap(), b"id,\xff\nx,Current\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_hardlinked_private_generation_still_excludes_its_owned_directory() {
        let parent = tempfile::tempdir().unwrap();
        let logical = parent.path().join("main.csv");
        let additional = parent.path().join("additional.csv");
        fs::write(&logical, b"id,value\nx,Original\n").unwrap();
        fs::write(&additional, b"id,city\nx,Paris\n").unwrap();
        let directory = Arc::new(tempfile::tempdir_in(parent.path()).unwrap());
        let path = directory.path().join("working.csv");
        fs::hard_link(&logical, &path).unwrap();
        let original = session(&logical, b',', true, None);
        let source = JoinWorkingSource::capture(
            session(&path, b',', true, None),
            &original,
            Arc::new(BTreeMap::new()),
            Arc::new(BTreeMap::new()),
            directory.clone(),
        );
        let plan = analyze(
            [input(&logical, 0, true), input(&additional, 0, true)],
            [Some(source), None],
            parent.path(),
        )
        .unwrap()
        .unwrap();
        let output = directory.path().join("joined.csv");
        assert!(plan.start_write(output.clone()).unwrap().wait().is_err());
        assert!(!output.exists());
        assert_eq!(fs::read(logical).unwrap(), b"id,value\nx,Original\n");
    }
}
