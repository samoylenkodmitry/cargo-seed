use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::git::Checkout;
use crate::tree_copy::copy_times;

/// Gives every file and directory that is the same in both trees the seed's times.
///
/// Git writes every file of a new worktree with the current time, so cargo
/// would call every unit of a cloned target stale. A path keeps its new time,
/// and its units rebuild, unless its committed content equals the seed's and
/// neither tree edited it. A directory counts as the same when its committed
/// tree is equal and it holds the same names: build scripts watch
/// directories, and cargo reads the times of the entries in them.
///
/// Returns how many paths took the seed's times.
pub(crate) fn copy_mtimes(seed: &Path, tree: &Path) -> io::Result<usize> {
    let (seed_git, tree_git) = (Checkout::new(seed), Checkout::new(tree));
    let seed_entries = seed_git.committed_entries()?;
    let mut edited = seed_git.edited_paths()?;
    edited.extend(tree_git.edited_paths()?);
    let mut copied = 0;
    for (path, entry) in tree_git.committed_entries()? {
        if seed_entries.get(&path) != Some(&entry) || edited.contains(&path) {
            continue;
        }
        let (seed_path, tree_path) = (join(seed, &path), join(tree, &path));
        if entry.kind == b"tree" && names(&seed_path)? != names(&tree_path)? {
            continue;
        }
        copy_times(&seed_path, &tree_path)?;
        copied += 1;
    }
    Ok(copied)
}

fn names(dir: &Path) -> io::Result<BTreeSet<OsString>> {
    fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect()
}

/// A git path, slash-separated bytes, under `root`.
#[cfg(unix)]
fn join(root: &Path, path: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    root.join(std::ffi::OsStr::from_bytes(path))
}

#[cfg(windows)]
fn join(root: &Path, path: &[u8]) -> PathBuf {
    String::from_utf8_lossy(path)
        .split('/')
        .fold(root.to_path_buf(), |joined, part| joined.join(part))
}
