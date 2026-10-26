use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

/// Every cargo target under `root`: a directory carrying cargo's cache tag.
/// The walk skips `.git` and `node_modules` and does not enter a target.
pub(crate) fn cargo_targets(root: &Path) -> io::Result<Vec<PathBuf>> {
    const SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let tag = dir.join("CACHEDIR.TAG");
        if fs::read_to_string(&tag).is_ok_and(|text| text.starts_with(SIGNATURE)) {
            found.push(dir);
            continue;
        }
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            if entry.file_type().is_ok_and(|kind| kind.is_dir())
                && name != ".git"
                && name != "node_modules"
            {
                pending.push(entry.path());
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The newest mtime of the target's profile directories and their
/// `.fingerprint`, which cargo rewrites whenever it builds anything.
pub(crate) fn newest_build(target: &Path) -> io::Result<Option<SystemTime>> {
    let mut newest = None;
    for profile in profile_dirs(target)? {
        for dir in [profile.clone(), profile.join(".fingerprint")] {
            if let Ok(modified) = fs::metadata(&dir).and_then(|meta| meta.modified()) {
                newest = newest.max(Some(modified));
            }
        }
    }
    Ok(newest)
}
