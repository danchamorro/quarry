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

/// Persisted user-selected temporary scratch folder across restarts (ADR 0005 amendment).
/// Only stores an absolute custom folder path; `None` means the OS default temporary folder.
#[derive(Debug, Default)]
pub struct StoragePreferences {
    pub scratch_directory: Option<PathBuf>,
    pub load_error: Option<String>,
    path: Option<PathBuf>,
    persist_enabled: bool,
}

impl StoragePreferences {
    /// Loads preferences from an explicit path without failing when `path` is `None`.
    pub fn load_from(path: Option<PathBuf>, persist_enabled: bool) -> Self {
        let mut scratch_directory = None;
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
                                            match parse_scratch_directory_pref(&content) {
                                                Some(dir) => scratch_directory = Some(dir),
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

    /// Updates the in-memory preference and persists it atomically as a private file.
    pub fn set_scratch_directory(&mut self, directory: Option<&Path>) -> std::io::Result<()> {
        let encoded = match directory {
            Some(dir) => {
                if !dir.is_absolute() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Temporary folder must be an absolute path.",
                    ));
                }
                let encoded = encode_scratch_directory_pref(dir)?;
                self.scratch_directory = Some(dir.to_path_buf());
                encoded
            }
            None => {
                self.scratch_directory = None;
                String::new()
            }
        };
        self.load_error = None;
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

fn parse_scratch_directory_pref(content: &str) -> Option<PathBuf> {
    for line in content.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("scratch_directory=")
            && !rest.is_empty()
            && !rest.contains('\0')
        {
            let candidate = PathBuf::from(rest);
            if candidate.is_absolute() {
                return Some(candidate);
            }
        }
        #[cfg(unix)]
        if let Some(hex) = line.strip_prefix("scratch_directory_hex=") {
            use std::os::unix::ffi::OsStringExt;
            let hex = hex.trim();
            if !hex.is_empty() && hex.len().is_multiple_of(2) {
                let mut bytes = Vec::with_capacity(hex.len() / 2);
                let mut valid = true;
                for pair in hex.as_bytes().as_chunks::<2>().0 {
                    let Ok(s) = std::str::from_utf8(pair) else {
                        valid = false;
                        break;
                    };
                    let Ok(byte) = u8::from_str_radix(s, 16) else {
                        valid = false;
                        break;
                    };
                    bytes.push(byte);
                }
                if valid && !bytes.contains(&0) {
                    let candidate = PathBuf::from(std::ffi::OsString::from_vec(bytes));
                    if candidate.is_absolute() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
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
        let reloaded = StoragePreferences::load_from(Some(pref_path.clone()), true);
        assert_eq!(
            reloaded.scratch_directory.as_deref(),
            Some(scratch.as_path())
        );
        assert!(reloaded.load_error.is_none());

        prefs.set_scratch_directory(None).unwrap();
        let cleared = StoragePreferences::load_from(Some(pref_path), true);
        assert!(cleared.scratch_directory.is_none());
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
