use eframe::egui;

const FEEDBACK_URL: &str =
    "https://github.com/danchamorro/quarry/issues/new?template=bug_report.yml";
const GUIDE_URL: &str = "https://github.com/danchamorro/quarry/blob/main/docs/USER_GUIDE.md";

pub(crate) struct About {
    pub(crate) open: bool,
    copied: bool,
    version: String,
    build: Option<String>,
    revision: Option<String>,
    source_status: Option<String>,
}

impl About {
    pub(crate) fn new() -> Self {
        Self {
            open: false,
            copied: false,
            version: bundle_value("CFBundleShortVersionString")
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
            build: bundle_value("CFBundleVersion"),
            revision: bundle_value("QuarryGitRevision"),
            source_status: bundle_value("QuarrySourceStatus"),
        }
    }

    pub(crate) fn open(&mut self) {
        self.open = true;
        self.copied = false;
    }

    fn details(&self) -> String {
        format!(
            "Quarry {} (Beta)\nBuild: {}\nRevision: {}\nSource status: {}\nPlatform: {} / {}",
            self.version,
            self.build.as_deref().unwrap_or("source build"),
            self.revision.as_deref().unwrap_or("not recorded"),
            self.source_status.as_deref().unwrap_or("not recorded"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    }

    pub(crate) fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("quarry-about"))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("About Quarry");
                ui.label(format!("Version {} · Beta", self.version));
                if let Some(build) = &self.build {
                    ui.label(format!("Build {build}"));
                } else {
                    ui.label("Source build");
                }
                if let Some(revision) = &self.revision {
                    ui.label(format!(
                        "Revision {} · {}",
                        revision.chars().take(12).collect::<String>(),
                        self.source_status.as_deref().unwrap_or("not recorded"),
                    ));
                }
                ui.horizontal(|ui| {
                    if ui.button("Copy build details").clicked() {
                        ctx.copy_text(self.details());
                        self.copied = true;
                    }
                    if self.copied {
                        ui.weak("Copied");
                    }
                });
                ui.separator();
                if ui.button("Report a bug or share feedback").clicked() {
                    ctx.open_url(egui::OpenUrl::new_tab(FEEDBACK_URL));
                }
                ui.label("Include build details, your macOS version, steps to reproduce, and expected and actual results.");
                ui.label("GitHub reports are public. Use a small synthetic example and remove private data from screenshots and logs.");
                if ui.button("User guide").clicked() {
                    ctx.open_url(egui::OpenUrl::new_tab(GUIDE_URL));
                }
                ui.add_space(8.0);
                close = ui.button("Close").clicked();
            });
        if close || modal.should_close() {
            self.open = false;
        }
    }
}

#[cfg(target_os = "macos")]
fn bundle_value(key: &str) -> Option<String> {
    use objc2_foundation::{NSBundle, NSString};

    let bundle = NSBundle::mainBundle();
    if bundle.bundleIdentifier()?.to_string() != super::APP_ID {
        return None;
    }
    bundle
        .objectForInfoDictionaryKey(&NSString::from_str(key))?
        .downcast::<NSString>()
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn bundle_value(_key: &str) -> Option<String> {
    None
}
