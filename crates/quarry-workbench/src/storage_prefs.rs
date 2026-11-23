//! Frontend-neutral persistent temporary-storage preferences, storage inspection,
//! and sidecar index-cache helpers shared across desktop frontends.

use crate::format_bytes;
use quarry_core::{StorageSpace, inspect_storage, validate_scratch_directory};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Adaptive per-job memory budget enforced by the Phase 3 out-of-core engine (300 MB).
pub const ADAPTIVE_MEMORY_BUDGET_BYTES: u64 = 300 * 1000 * 1000;

const MAX_STORAGE_PREFERENCES_BYTES: u64 = 8 * 1024;

/// Ignore unusable environment roots instead of writing preferences beneath the working directory.
fn unix_storage_preferences_path(
    config_root: Option<PathBuf>,
    user_root: Option<PathBuf>,
) -> Option<PathBuf> {
    config_root
        .filter(|root| root.is_absolute())
        .or_else(|| {
            user_root
                .filter(|root| root.is_absolute())
                .map(|root| root.join(".config"))
        })
        .map(|root| root.join("quarry/storage-preferences"))
}

/// Persisted user-selected temporary scratch folder and sidecar index-cache preference
/// across restarts (ADR 0005 amendment).
/// `scratch_directory = None` means the OS default temporary folder; `index_cache_enabled = None`
/// means the preference was not explicitly written (defaults to `true`).
#[derive(Debug, Default)]
pub struct StoragePreferences {
    pub scratch_directory: Option<PathBuf>,
    pub index_cache_enabled: Option<bool>,
    pub load_error: Option<String>,
    path: Option<PathBuf>,
    persist_enabled: bool,
}

impl StoragePreferences {
    /// Effective sidecar index-cache setting (defaults to `true` when unset).
    pub fn index_cache_enabled(&self) -> bool {
        self.index_cache_enabled.unwrap_or(true)
    }

    /// Loads preferences from an explicit path without failing when `path` is `None`.
    pub fn load_from(path: Option<PathBuf>, persist_enabled: bool) -> Self {
        let mut scratch_directory = None;
        let mut index_cache_enabled = None;
        let mut load_error = None;
        if let Some(pref_path) = path.as_ref() {
            match std::fs::File::open(pref_path) {
                Ok(file) => {
                    let mut raw = Vec::new();
                    match file
                        .take(MAX_STORAGE_PREFERENCES_BYTES + 1)
                        .read_to_end(&mut raw)
                    {
                        Ok(_) => {
                            if raw.len() as u64 > MAX_STORAGE_PREFERENCES_BYTES {
                                load_error = Some(format!(
                                    "Saved temporary folder preference ({}) exceeds the maximum supported size. Open File > Temporary Storage… to choose your temporary folder again.",
                                    pref_path.display()
                                ));
                            } else {
                                match String::from_utf8(raw) {
                                    Ok(content) => {
                                        if !content.trim().is_empty() {
                                            match parse_storage_prefs(&content) {
                                                Some((dir, cache)) => {
                                                    scratch_directory = dir;
                                                    index_cache_enabled = cache;
                                                }
                                                None => {
                                                    load_error = Some(format!(
                                                        "Saved temporary folder preference ({}) could not be parsed. Open File > Temporary Storage… to choose your temporary folder again.",
                                                        pref_path.display()
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        load_error = Some(format!(
                                            "Saved temporary folder preference ({}) could not be parsed. Open File > Temporary Storage… to choose your temporary folder again.",
                                            pref_path.display()
                                        ));
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            load_error = Some(format!(
                                "Could not read saved temporary folder preference ({}): {error}. Open File > Temporary Storage… to choose your temporary folder again.",
                                pref_path.display()
                            ));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    load_error = Some(format!(
                        "Could not read saved temporary folder preference ({}): {error}. Open File > Temporary Storage… to choose your temporary folder again.",
                        pref_path.display()
                    ));
                }
            }
        }
        Self {
            scratch_directory,
            index_cache_enabled,
            load_error,
            path,
            persist_enabled,
        }
    }

    /// Loads application-wide preferences from the standard per-user config directory.
    pub fn application() -> Self {
        let path = if cfg!(target_os = "macos") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Library/Application Support/Quarry/storage-preferences"))
        } else if cfg!(target_os = "windows") {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Quarry/storage-preferences"))
        } else {
            unix_storage_preferences_path(
                std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
                std::env::var_os("HOME").map(PathBuf::from),
            )
        };
        Self::load_from(path, true)
    }

    fn persist_current(&self) -> std::io::Result<()> {
        let mut encoded = String::new();
        if let Some(dir) = self.scratch_directory.as_deref() {
            encoded.push_str(&encode_scratch_directory_pref(dir)?);
        }
        if let Some(enabled) = self.index_cache_enabled {
            encoded.push_str(if enabled {
                "index_cache_enabled=true\n"
            } else {
                "index_cache_enabled=false\n"
            });
        }
        if encoded.len() as u64 > MAX_STORAGE_PREFERENCES_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Storage preferences exceed the maximum supported size.",
            ));
        }
        let Some(path) = &self.path else {
            if self.persist_enabled {
                return Err(std::io::Error::other(
                    "no writable configuration directory is available for saving preferences across restarts",
                ));
            }
            return Ok(());
        };
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("Invalid storage preference location"))?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        quarry_core::restrict_private_file(file.path(), file.as_file())?;
        file.write_all(encoded.as_bytes())?;
        file.flush()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Updates the in-memory scratch folder preference and persists it atomically.
    pub fn set_scratch_directory(&mut self, directory: Option<&Path>) -> std::io::Result<()> {
        if let Some(dir) = directory
            && !dir.is_absolute()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Temporary folder must be an absolute path.",
            ));
        }
        let previous = self.scratch_directory.clone();
        self.scratch_directory = directory.map(Path::to_path_buf);
        if let Err(error) = self.persist_current() {
            self.scratch_directory = previous;
            return Err(error);
        }
        self.load_error = None;
        Ok(())
    }

    /// Updates the persistent sidecar index-cache toggle and saves it atomically.
    pub fn set_index_cache_enabled(&mut self, enabled: bool) -> std::io::Result<()> {
        let previous = self.index_cache_enabled;
        self.index_cache_enabled = Some(enabled);
        if let Err(error) = self.persist_current() {
            self.index_cache_enabled = previous;
            return Err(error);
        }
        self.load_error = None;
        Ok(())
    }
}

fn encode_scratch_directory_pref(path: &Path) -> std::io::Result<String> {
    let encoded = if let Some(text) = path.to_str()
        && !text.contains('\n')
        && !text.contains('\r')
        && !text.contains('\0')
        && text.trim() == text
    {
        format!("scratch_directory={text}\n")
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bytes = path.as_os_str().as_bytes();
            let mut hex =
                String::with_capacity("scratch_directory_hex=".len() + bytes.len() * 2 + 1);
            hex.push_str("scratch_directory_hex=");
            for byte in bytes {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
            }
            hex.push('\n');
            hex
        }
        #[cfg(not(unix))]
        {
            if let Some(text) = path.to_str()
                && !text.contains('\n')
                && !text.contains('\r')
                && !text.contains('\0')
            {
                format!("scratch_directory={text}\n")
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Temporary folder path contains unsupported characters.",
                ));
            }
        }
    };
    if encoded.len() as u64 > MAX_STORAGE_PREFERENCES_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Temporary folder path is too long to save across restarts.",
        ));
    }
    Ok(encoded)
}

fn parse_storage_prefs(content: &str) -> Option<(Option<PathBuf>, Option<bool>)> {
    let mut scratch = None;
    let mut cache = None;
    let mut parsed_any = false;
    for line in content.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("scratch_directory=") {
            if rest.is_empty() || rest.contains('\0') {
                return None;
            }
            let candidate = PathBuf::from(rest);
            if !candidate.is_absolute() {
                return None;
            }
            scratch = Some(candidate);
            parsed_any = true;
            continue;
        }
        #[cfg(unix)]
        if let Some(hex) = line.strip_prefix("scratch_directory_hex=") {
            use std::os::unix::ffi::OsStringExt;
            let hex = hex.trim();
            if hex.is_empty() || !hex.len().is_multiple_of(2) {
                return None;
            }
            let mut bytes = Vec::with_capacity(hex.len() / 2);
            for pair in hex.as_bytes().as_chunks::<2>().0 {
                let s = std::str::from_utf8(pair).ok()?;
                let byte = u8::from_str_radix(s, 16).ok()?;
                bytes.push(byte);
            }
            if bytes.contains(&0) {
                return None;
            }
            let candidate = PathBuf::from(std::ffi::OsString::from_vec(bytes));
            if !candidate.is_absolute() {
                return None;
            }
            scratch = Some(candidate);
            parsed_any = true;
            continue;
        }
        if let Some(val) = line.strip_prefix("index_cache_enabled=") {
            match val.trim() {
                "true" => cache = Some(true),
                "false" => cache = Some(false),
                _ => return None,
            }
            parsed_any = true;
            continue;
        }
        return None;
    }
    parsed_any.then_some((scratch, cache))
}

/// Inspects available disk space for `directory` (validating custom scratch folders) and sums
/// the on-disk size of `retained_paths`.
pub fn inspect_temporary_storage(
    directory: &Path,
    validate_custom_scratch: bool,
    retained_paths: &[PathBuf],
) -> Result<(StorageSpace, u64), String> {
    let space = if validate_custom_scratch {
        validate_scratch_directory(directory)
    } else {
        inspect_storage(directory)
    }
    .map_err(|error| error.to_string())?;
    let retained_bytes = retained_paths.iter().try_fold(0_u64, |bytes, path| {
        std::fs::metadata(path)
            .map(|metadata| bytes.saturating_add(metadata.len()))
            .map_err(|error| {
                format!(
                    "Cannot inspect retained working file {}: {error}. Reconnect its drive before continuing.",
                    path.display()
                )
            })
    })?;
    Ok((space, retained_bytes))
}

/// Clears the default persistent sidecar index cache and returns a user-facing summary.
pub fn clear_index_cache_summary() -> Result<String, String> {
    quarry_core::clear_index_cache(None)
        .map(|summary| {
            if summary.files_removed == 0 {
                "Index cache is already empty.".into()
            } else {
                format!(
                    "Cleared {} cached index file{} ({}).",
                    summary.files_removed,
                    if summary.files_removed == 1 { "" } else { "s" },
                    format_bytes(summary.bytes_reclaimed)
                )
            }
        })
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_preferences_round_trip_and_reject_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let pref_path = dir.path().join("prefs/storage-preferences");
        let scratch = dir.path().join("custom scratch ");

        let mut prefs = StoragePreferences::load_from(Some(pref_path.clone()), true);
        assert!(prefs.scratch_directory.is_none());
        assert!(
            prefs
                .set_scratch_directory(Some(Path::new("relative/dir")))
                .is_err()
        );

        prefs.set_scratch_directory(Some(&scratch)).unwrap();
        prefs.set_index_cache_enabled(false).unwrap();
        let reloaded = StoragePreferences::load_from(Some(pref_path.clone()), true);
        assert_eq!(
            reloaded.scratch_directory.as_deref(),
            Some(scratch.as_path())
        );
        assert_eq!(reloaded.index_cache_enabled, Some(false));
        assert!(!reloaded.index_cache_enabled());
        assert!(reloaded.load_error.is_none());

        prefs.set_scratch_directory(None).unwrap();
        let cleared = StoragePreferences::load_from(Some(pref_path), true);
        assert!(cleared.scratch_directory.is_none());
        assert_eq!(cleared.index_cache_enabled, Some(false));
    }

    #[test]
    fn inspect_temporary_storage_validates_directory_and_sums_retained_files() {
        let dir = tempfile::tempdir().unwrap();
        let retained = dir.path().join("working.csv");
        std::fs::write(&retained, b"a,b\n1,2\n").unwrap();
        let (space, bytes) = inspect_temporary_storage(dir.path(), true, &[retained]).unwrap();
        assert!(space.available_bytes > 0);
        assert_eq!(bytes, 8);
    }
}
