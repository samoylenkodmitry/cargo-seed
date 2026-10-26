use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The directories of a cargo target that hold a `.fingerprint`:
/// `<target>/<profile>` and `<target>/<triple>/<profile>`.
pub(crate) fn profile_dirs(target: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for level in fs::read_dir(target)? {
        let level = level?.path();
        if !level.is_dir() {
            continue;
        }
        if level.join(".fingerprint").is_dir() {
            found.push(level.clone());
        }
        for nested in fs::read_dir(&level)?.flatten() {
            if nested.path().join(".fingerprint").is_dir() {
                found.push(nested.path());
            }
        }
    }
    Ok(found)
}

/// One directory per crate unit with rustc incremental state, in every profile.
pub(crate) fn incremental_crate_dirs(target: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for profile in profile_dirs(target)? {
        for crate_dir in fs::read_dir(profile.join("incremental"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if crate_dir.file_type()?.is_dir() {
                found.push(crate_dir.path());
            }
        }
    }
    Ok(found)
}
