#![deny(unsafe_op_in_unsafe_fn)]

use std::cell::{OnceCell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSApplication,
    NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSButton, NSColor, NSFont, NSProgressIndicator, NSProgressIndicatorStyle,
    NSScrollView, NSTextField, NSTextView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString, NSTimer,
};
use quarry_core::{
    IndexConfig, IndexJob, IndexProgress, OpenOptions, Row, Session, StructuralIndex,
};

const VIEWPORT_ROWS: usize = 100;
const MAX_VISIBLE_COLUMNS: usize = 32;
const RAIL_BOTTOM: f64 = 54.0;
const RAIL_HEIGHT: f64 = 526.0;

struct AppDelegateIvars {
    initial_path: Option<PathBuf>,
    started: Instant,
    document: RefCell<Option<Document>>,
    notice: RefCell<Option<String>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    window: OnceCell<Retained<NSWindow>>,
    path_field: OnceCell<Retained<NSTextField>>,
    jump_field: OnceCell<Retained<NSTextField>>,
    progress: OnceCell<Retained<NSProgressIndicator>>,
    cancel_button: OnceCell<Retained<NSButton>>,
    rows_label: OnceCell<Retained<NSTextField>>,
    grid_text: OnceCell<Retained<NSTextView>>,
    status_label: OnceCell<Retained<NSTextField>>,
    rail_fill: OnceCell<Retained<NSTextField>>,
    rail_marker: OnceCell<Retained<NSTextField>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppDelegateIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, notification: &NSNotification) {
            self.build_window();
            if self.ivars().initial_path.is_some() {
                self.open_current_path();
            } else {
                self.render();
            }

            let window = self.ivars().window.get().expect("window is initialized");
            window.makeKeyAndOrderFront(None);

            let app = notification
                .object()
                .expect("launch notification has an application")
                .downcast::<NSApplication>()
                .expect("notification object is NSApplication");
            app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);

            eprintln!(
                "quarry-appkit first window: {:.3} ms",
                self.ivars().started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    unsafe impl NSWindowDelegate for AppDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }

    impl AppDelegate {
        #[unsafe(method(openDocument:))]
        fn open_document_action(&self, _sender: Option<&AnyObject>) {
            self.open_current_path();
        }

        #[unsafe(method(previousPage:))]
        fn previous_page(&self, _sender: Option<&AnyObject>) {
            self.navigate(Navigation::Previous);
        }

        #[unsafe(method(nextPage:))]
        fn next_page(&self, _sender: Option<&AnyObject>) {
            self.navigate(Navigation::Next);
        }

        #[unsafe(method(jumpToRow:))]
        fn jump_to_row(&self, _sender: Option<&AnyObject>) {
            let value = self
                .ivars()
                .jump_field
                .get()
                .expect("jump field is initialized")
                .stringValue()
                .to_string();
            self.navigate(Navigation::Jump(value));
        }

        #[unsafe(method(cancelIndex:))]
        fn cancel_index(&self, _sender: Option<&AnyObject>) {
            if let Some(document) = self.ivars().document.borrow().as_ref() {
                document.cancel();
            }
            self.render();
        }

        #[unsafe(method(pollIndex:))]
        fn poll_index(&self, _timer: &NSTimer) {
            let result = self
                .ivars()
                .document
                .borrow_mut()
                .as_mut()
                .map(Document::poll)
                .transpose();
            if let Err(error) = result {
                *self.ivars().notice.borrow_mut() = Some(error);
            }
            self.render();
            if self
                .ivars()
                .document
                .borrow()
                .as_ref()
                .is_none_or(|document| !document.is_indexing())
            {
                self.stop_timer();
            }
        }
    }
);

impl AppDelegate {
    fn new(
        initial_path: Option<PathBuf>,
        started: Instant,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppDelegateIvars {
            initial_path,
            started,
            document: RefCell::new(None),
            notice: RefCell::new(None),
            timer: RefCell::new(None),
            window: OnceCell::new(),
            path_field: OnceCell::new(),
            jump_field: OnceCell::new(),
            progress: OnceCell::new(),
            cancel_button: OnceCell::new(),
            rows_label: OnceCell::new(),
            grid_text: OnceCell::new(),
            status_label: OnceCell::new(),
            rail_fill: OnceCell::new(),
            rail_marker: OnceCell::new(),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn build_window(&self) {
        let mtm = self.mtm();
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, 1280.0, 780.0),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setAppearance(
            unsafe { NSAppearance::appearanceNamed(NSAppearanceNameAqua) }.as_deref(),
        );
        window.setTitle(&ns("Quarry — AppKit prototype"));
        window.setContentMinSize(NSSize::new(860.0, 540.0));
        window.setBackgroundColor(Some(&color(0.957, 0.969, 0.973)));
        window.center();
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let content = window.contentView().expect("window has a content view");

        let brand = label("QUARRY", rect(16.0, 742.0, 104.0, 25.0), 21.0, true, mtm);
        let candidate = label(
            "APPKIT BAKE-OFF",
            rect(112.0, 746.0, 150.0, 18.0),
            10.0,
            false,
            mtm,
        );
        candidate.setTextColor(Some(&color(0.192, 0.333, 0.851)));
        let read_only = label(
            "READ ONLY",
            rect(1174.0, 746.0, 88.0, 18.0),
            10.0,
            false,
            mtm,
        );
        read_only.setAlignment(objc2_app_kit::NSTextAlignment::Right);
        read_only.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        content.addSubview(&brand);
        content.addSubview(&candidate);
        content.addSubview(&read_only);

        let file_label = label("File", rect(16.0, 706.0, 34.0, 24.0), 13.0, false, mtm);
        let initial_path = self
            .ivars()
            .initial_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path_field = NSTextField::textFieldWithString(&ns(&initial_path), mtm);
        path_field.setFrame(rect(52.0, 704.0, 1108.0, 28.0));
        path_field.setPlaceholderString(Some(&ns("/path/to/file.csv")));
        path_field.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        set_accessibility_label(&*path_field, "File");
        let open = button(
            "Open",
            rect(1170.0, 703.0, 94.0, 30.0),
            self,
            sel!(openDocument:),
            mtm,
        );
        open.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        content.addSubview(&file_label);
        content.addSubview(&path_field);
        content.addSubview(&open);

        let progress = NSProgressIndicator::initWithFrame(
            NSProgressIndicator::alloc(mtm),
            rect(16.0, 667.0, 1144.0, 16.0),
        );
        progress.setStyle(NSProgressIndicatorStyle::Bar);
        progress.setIndeterminate(false);
        progress.setMinValue(0.0);
        progress.setMaxValue(1.0);
        progress.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        let cancel = button(
            "Cancel",
            rect(1170.0, 660.0, 94.0, 30.0),
            self,
            sel!(cancelIndex:),
            mtm,
        );
        cancel.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        content.addSubview(&progress);
        content.addSubview(&cancel);

        let previous = button(
            "Previous",
            rect(16.0, 620.0, 92.0, 30.0),
            self,
            sel!(previousPage:),
            mtm,
        );
        previous.setKeyEquivalent(&ns("\u{f72c}"));
        let next = button(
            "Next",
            rect(112.0, 620.0, 72.0, 30.0),
            self,
            sel!(nextPage:),
            mtm,
        );
        next.setKeyEquivalent(&ns("\u{f72d}"));
        let jump_label = label("Data row", rect(196.0, 624.0, 62.0, 22.0), 13.0, false, mtm);
        let jump_field = NSTextField::textFieldWithString(&ns("1"), mtm);
        jump_field.setFrame(rect(262.0, 621.0, 116.0, 28.0));
        set_accessibility_label(&*jump_field, "Data row");
        let jump = button(
            "Jump",
            rect(386.0, 620.0, 72.0, 30.0),
            self,
            sel!(jumpToRow:),
            mtm,
        );
        let keys = label(
            "Page Up / Page Down",
            rect(470.0, 625.0, 180.0, 18.0),
            10.0,
            false,
            mtm,
        );
        previous.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        next.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        jump_label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        jump_field.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        jump.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        keys.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        content.addSubview(&previous);
        content.addSubview(&next);
        content.addSubview(&jump_label);
        content.addSubview(&jump_field);
        content.addSubview(&jump);
        content.addSubview(&keys);

        let file = label("FILE", rect(8.0, 584.0, 32.0, 16.0), 9.0, false, mtm);
        let rail_background = colored_strip(
            rect(16.0, RAIL_BOTTOM, 10.0, RAIL_HEIGHT),
            color(0.824, 0.859, 0.875),
            mtm,
        );
        let rail_fill = colored_strip(
            rect(18.0, RAIL_BOTTOM + RAIL_HEIGHT, 6.0, 0.0),
            color(0.31, 0.498, 0.514),
            mtm,
        );
        let rail_marker = colored_strip(
            rect(13.0, RAIL_BOTTOM + RAIL_HEIGHT - 2.0, 16.0, 3.0),
            color(0.192, 0.333, 0.851),
            mtm,
        );
        for view in [&*file, &*rail_background, &*rail_fill, &*rail_marker] {
            content.addSubview(view);
        }

        let rows_label = label(
            "Open a delimited file",
            rect(42.0, 584.0, 900.0, 28.0),
            18.0,
            true,
            mtm,
        );
        rows_label.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(42.0, RAIL_BOTTOM, 1222.0, RAIL_HEIGHT),
        );
        scroll.setHasHorizontalScroller(true);
        scroll.setHasVerticalScroller(true);
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let content_size = scroll.contentSize();
        let grid_text = NSTextView::initWithFrame(
            NSTextView::alloc(mtm),
            rect(0.0, 0.0, content_size.width, content_size.height),
        );
        scroll.setDocumentView(Some(&grid_text));
        grid_text.setEditable(false);
        grid_text.setSelectable(true);
        grid_text.setDrawsBackground(true);
        grid_text.setBackgroundColor(&color(0.975, 0.982, 0.984));
        grid_text.setFont(NSFont::userFixedPitchFontOfSize(13.0).as_deref());
        grid_text.setMinSize(NSSize::new(0.0, content_size.height));
        grid_text.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
        grid_text.setHorizontallyResizable(true);
        grid_text.setVerticallyResizable(true);
        grid_text.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let text_container =
            unsafe { grid_text.textContainer() }.expect("text view has a text container");
        text_container.setWidthTracksTextView(false);
        text_container.setSize(NSSize::new(f64::MAX, f64::MAX));
        set_accessibility_label(&*grid_text, "Data viewport");
        content.addSubview(&rows_label);
        content.addSubview(&scroll);

        let status = label(
            "No file open · pass a path or paste one above",
            rect(16.0, 14.0, 1248.0, 22.0),
            12.0,
            false,
            mtm,
        );
        status.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        content.addSubview(&status);

        set_once(&self.ivars().window, window);
        set_once(&self.ivars().path_field, path_field);
        set_once(&self.ivars().jump_field, jump_field);
        set_once(&self.ivars().progress, progress);
        set_once(&self.ivars().cancel_button, cancel);
        set_once(&self.ivars().rows_label, rows_label);
        set_once(&self.ivars().grid_text, grid_text);
        set_once(&self.ivars().status_label, status);
        set_once(&self.ivars().rail_fill, rail_fill);
        set_once(&self.ivars().rail_marker, rail_marker);
    }

    fn open_current_path(&self) {
        let path = self
            .ivars()
            .path_field
            .get()
            .expect("path field is initialized")
            .stringValue()
            .to_string();
        if path.trim().is_empty() {
            *self.ivars().notice.borrow_mut() = Some("Enter a file path to open.".into());
            self.render();
            return;
        }

        match Document::open(Path::new(path.trim())) {
            Ok(document) => {
                self.stop_timer();
                *self.ivars().document.borrow_mut() = Some(document);
                *self.ivars().notice.borrow_mut() = None;
                self.ivars()
                    .jump_field
                    .get()
                    .expect("jump field is initialized")
                    .setStringValue(&ns("1"));
                self.start_timer();
            }
            Err(error) => *self.ivars().notice.borrow_mut() = Some(error),
        }
        self.render();
    }

    fn navigate(&self, navigation: Navigation) {
        let result = self
            .ivars()
            .document
            .borrow_mut()
            .as_mut()
            .ok_or_else(|| "Open a file first.".to_owned())
            .and_then(|document| match navigation {
                Navigation::Previous => document.previous(),
                Navigation::Next => document.next(),
                Navigation::Jump(value) => document.jump(&value),
            });
        *self.ivars().notice.borrow_mut() = result.err();
        self.render();
    }

    fn start_timer(&self) {
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.1,
                self,
                sel!(pollIndex:),
                None,
                true,
            )
        };
        *self.ivars().timer.borrow_mut() = Some(timer);
    }

    fn stop_timer(&self) {
        if let Some(timer) = self.ivars().timer.borrow_mut().take() {
            timer.invalidate();
        }
    }

    fn render(&self) {
        let document = self.ivars().document.borrow();
        let notice = self.ivars().notice.borrow();
        let progress = self
            .ivars()
            .progress
            .get()
            .expect("progress is initialized");
        let cancel = self
            .ivars()
            .cancel_button
            .get()
            .expect("cancel button is initialized");
        let rows_label = self
            .ivars()
            .rows_label
            .get()
            .expect("rows label is initialized");
        let grid = self
            .ivars()
            .grid_text
            .get()
            .expect("grid text is initialized");
        let status = self
            .ivars()
            .status_label
            .get()
            .expect("status label is initialized");

        if let Some(document) = document.as_ref() {
            let fraction = document.indexed_fraction();
            progress.setDoubleValue(fraction);
            cancel.setEnabled(document.is_indexing());
            rows_label.setStringValue(&ns(&format!(
                "Rows {}–{}",
                document.display_start(),
                document.display_end()
            )));
            grid.setString(&ns(&grid_contents(document)));
            status.setStringValue(&ns(&status_text(document, notice.as_deref())));

            let indexed = RAIL_HEIGHT * fraction.clamp(0.0, 1.0);
            self.ivars()
                .rail_fill
                .get()
                .expect("rail fill is initialized")
                .setFrame(rect(
                    18.0,
                    RAIL_BOTTOM + RAIL_HEIGHT - indexed,
                    6.0,
                    indexed,
                ));
            self.ivars()
                .rail_marker
                .get()
                .expect("rail marker is initialized")
                .setFrame(rect(
                    13.0,
                    RAIL_BOTTOM + RAIL_HEIGHT
                        - RAIL_HEIGHT * document.viewport_fraction().clamp(0.0, 1.0)
                        - 2.0,
                    16.0,
                    3.0,
                ));
        } else {
            progress.setDoubleValue(0.0);
            cancel.setEnabled(false);
            rows_label.setStringValue(&ns("Open a delimited file"));
            grid.setString(&ns(
                "Quarry reads the first viewport before indexing the rest.",
            ));
            status.setStringValue(&ns(notice
                .as_deref()
                .unwrap_or("No file open · pass a path or paste one above")));
        }
    }
}

enum Navigation {
    Previous,
    Next,
    Jump(String),
}

struct Document {
    session: Session,
    job: Option<IndexJob>,
    index: Option<StructuralIndex>,
    progress: IndexProgress,
    headers: Vec<String>,
    total_columns: usize,
    data_start: u64,
    viewport_start: u64,
    rows: Vec<Row>,
    last_viewport_read: Option<Duration>,
}

impl Document {
    fn open(path: &Path) -> Result<Self, String> {
        let session = Session::open(
            path,
            OpenOptions {
                rows: VIEWPORT_ROWS + 1,
                ..OpenOptions::default()
            },
        )
        .map_err(|error| error.to_string())?;
        let data_start = u64::from(session.dialect.has_header);
        let total_columns = session
            .first_rows
            .iter()
            .map(|row| row.fields.len())
            .max()
            .unwrap_or(0);
        let headers = headers_for(&session, total_columns);
        let rows = session
            .first_rows
            .iter()
            .skip(data_start as usize)
            .take(VIEWPORT_ROWS)
            .cloned()
            .collect();
        let job = session
            .start_indexing(IndexConfig::default())
            .map_err(|error| error.to_string())?;
        let progress = job.progress();

        Ok(Self {
            session,
            job: Some(job),
            index: None,
            progress,
            headers,
            total_columns,
            data_start,
            viewport_start: data_start,
            rows,
            last_viewport_read: None,
        })
    }

    fn poll(&mut self) -> Result<(), String> {
        let Some(job) = &self.job else {
            return Ok(());
        };
        self.progress = job.progress();
        if self.progress.done {
            let job = self.job.take().expect("index job is present");
            self.index = Some(job.wait().map_err(|error| error.to_string())?);
        }
        Ok(())
    }

    fn is_indexing(&self) -> bool {
        self.job.is_some() && !self.progress.done
    }

    fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancel();
        }
    }

    fn previous(&mut self) -> Result<(), String> {
        self.navigate(
            self.viewport_start
                .saturating_sub(VIEWPORT_ROWS as u64)
                .max(self.data_start),
        )
    }

    fn next(&mut self) -> Result<(), String> {
        self.navigate(self.viewport_start.saturating_add(VIEWPORT_ROWS as u64))
    }

    fn jump(&mut self, value: &str) -> Result<(), String> {
        self.navigate(parse_data_row(value, self.data_start)?)
    }

    fn navigate(&mut self, requested: u64) -> Result<(), String> {
        let available = self.available_rows();
        let Some((start, count)) = viewport_request(requested, available, self.data_start) else {
            return Err(format!(
                "Data row {} is not indexed yet ({} available).",
                requested.saturating_sub(self.data_start).saturating_add(1),
                self.available_data_rows()
            ));
        };
        let began = Instant::now();
        let rows = if let Some(index) = &self.index {
            self.session.read_rows(index, start, count)
        } else if let Some(job) = &self.job {
            self.session.read_rows(&job.snapshot(), start, count)
        } else {
            return Err("No structural index is available.".into());
        }
        .map_err(|error| error.to_string())?;
        self.last_viewport_read = Some(began.elapsed());
        self.viewport_start = start;
        self.rows = rows;
        Ok(())
    }

    fn available_rows(&self) -> u64 {
        self.index
            .as_ref()
            .map(StructuralIndex::indexed_rows)
            .unwrap_or(self.progress.rows_scanned)
    }

    fn available_data_rows(&self) -> u64 {
        self.available_rows().saturating_sub(self.data_start)
    }

    fn indexed_fraction(&self) -> f64 {
        self.progress.bytes_scanned as f64 / self.session.file_size.max(1) as f64
    }

    fn viewport_fraction(&self) -> f64 {
        self.rows
            .first()
            .map(|row| row.offset as f64 / self.session.file_size.max(1) as f64)
            .unwrap_or(0.0)
    }

    fn display_start(&self) -> u64 {
        self.viewport_start
            .saturating_sub(self.data_start)
            .saturating_add(1)
    }

    fn display_end(&self) -> u64 {
        self.display_start()
            .saturating_add(self.rows.len().saturating_sub(1) as u64)
    }
}

fn viewport_request(requested: u64, available: u64, data_start: u64) -> Option<(u64, usize)> {
    let start = requested.max(data_start);
    if start >= available {
        return None;
    }
    Some((
        start,
        (available - start).min(VIEWPORT_ROWS as u64) as usize,
    ))
}

fn parse_data_row(value: &str, data_start: u64) -> Result<u64, String> {
    let row: u64 = value
        .trim()
        .parse()
        .map_err(|_| "Data row must be a positive whole number.".to_owned())?;
    if row == 0 {
        return Err("Data rows start at 1.".into());
    }
    Ok(data_start.saturating_add(row - 1))
}

fn headers_for(session: &Session, total_columns: usize) -> Vec<String> {
    // ponytail: render 32 columns in this spike; add native column virtualization if AppKit wins.
    let visible = total_columns.min(MAX_VISIBLE_COLUMNS);
    if session.dialect.has_header {
        return session
            .first_rows
            .first()
            .map(|row| {
                row.fields
                    .iter()
                    .take(visible)
                    .enumerate()
                    .map(|(index, field)| {
                        let text = field_text(field, 48);
                        if text.is_empty() {
                            format!("Column {}", index + 1)
                        } else {
                            text
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    (1..=visible)
        .map(|index| format!("Column {index}"))
        .collect()
}

fn grid_contents(document: &Document) -> String {
    let mut output = String::from("ROW  ");
    output.push_str(&document.headers.join("  "));
    output.push('\n');
    let start = document.display_start();
    for (index, row) in document.rows.iter().enumerate() {
        output.push_str(&(start + index as u64).to_string());
        for column in 0..document.headers.len() {
            output.push_str("  ");
            if let Some(field) = row.fields.get(column) {
                output.push_str(&field_text(field, 48));
            }
        }
        output.push('\n');
    }
    output
}

fn status_text(document: &Document, notice: Option<&str>) -> String {
    if let Some(notice) = notice {
        return notice.to_owned();
    }
    let state = if document.progress.cancelled {
        "Index cancelled".to_owned()
    } else if document.is_indexing() {
        format!("Indexing {:.1}%", document.indexed_fraction() * 100.0)
    } else {
        "Index complete".to_owned()
    };
    let viewport = document
        .last_viewport_read
        .map(|duration| format!(" · viewport {:.3} ms", duration.as_secs_f64() * 1000.0))
        .unwrap_or_default();
    format!(
        "{}  |  {} columns  |  {} data rows indexed  |  first rows {:.3} ms  |  {state}{viewport}",
        format_bytes(document.session.file_size),
        document.total_columns,
        document.available_data_rows(),
        document.session.metrics.first_rows.as_secs_f64() * 1000.0,
    )
}

fn field_text(field: &[u8], limit: usize) -> String {
    let rendered = String::from_utf8_lossy(field)
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    if rendered.chars().count() <= limit {
        rendered
    } else {
        rendered.chars().take(limit - 3).collect::<String>() + "..."
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

fn ns(value: &str) -> Retained<NSString> {
    NSString::from_str(value)
}

fn color(red: f64, green: f64, blue: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(red, green, blue, 1.0)
}

fn label(
    text: &str,
    frame: NSRect,
    size: f64,
    bold: bool,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&ns(text), mtm);
    label.setFrame(frame);
    let font = if bold {
        NSFont::boldSystemFontOfSize(size)
    } else {
        NSFont::systemFontOfSize(size)
    };
    label.setFont(Some(&font));
    label
}

fn colored_strip(
    frame: NSRect,
    background: Retained<NSColor>,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let strip = NSTextField::labelWithString(&ns(""), mtm);
    strip.setFrame(frame);
    strip.setDrawsBackground(true);
    strip.setBackgroundColor(Some(&background));
    set_accessibility_hidden(&*strip);
    strip
}

fn button(
    title: &str,
    frame: NSRect,
    target: &AppDelegate,
    action: objc2::runtime::Sel,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(&ns(title), Some(target), Some(action), mtm)
    };
    button.setFrame(frame);
    button
}

fn set_accessibility_label(view: &impl objc2::Message, label: &str) {
    unsafe {
        let _: () = msg_send![view, setAccessibilityLabel: &*ns(label)];
    }
}

fn set_accessibility_hidden(view: &impl objc2::Message) {
    unsafe {
        let _: () = msg_send![view, setAccessibilityElement: false];
    }
}

fn set_once<T>(cell: &OnceCell<T>, value: T) {
    assert!(cell.set(value).is_ok(), "UI element initialized twice");
}

fn main() {
    let started = Instant::now();
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
    let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let delegate = AppDelegate::new(initial_path, started, mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
}

#[cfg(test)]
mod tests {
    use super::{VIEWPORT_ROWS, parse_data_row, viewport_request};

    #[test]
    fn viewport_requests_are_bounded_and_use_one_based_data_rows() {
        assert_eq!(viewport_request(0, 1_001, 1), Some((1, VIEWPORT_ROWS)));
        assert_eq!(viewport_request(950, 1_001, 1), Some((950, 51)));
        assert_eq!(viewport_request(1_001, 1_001, 1), None);
        assert_eq!(parse_data_row("1", 1).unwrap(), 1);
        assert_eq!(parse_data_row("100000000", 1).unwrap(), 100_000_000);
        assert_eq!(parse_data_row("0", 1).unwrap_err(), "Data rows start at 1.");
    }
}
