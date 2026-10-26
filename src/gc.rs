//! Whole cargo targets go, least recently built first.
//!
//! A target is a cache: a rebuild restores it. One unused for `max_idle`
//! goes whatever the disk holds, so a repository nobody works on costs
//! nothing; under `min_free`, more go, oldest first, until the disk has room.
//! Targets with a build running or finished recently stay: tests may still
//! run from them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::build_lock::BuildLock;
use crate::disk::{free_bytes, gigabytes};
use crate::target_dir::{cargo_targets, newest_build};

/// A target built within this long may have tests running from it.
const RECENT: Duration = Duration::from_secs(2 * 60 * 60);
/// What a removal is renamed to first, so a build starting at that moment
/// makes a new target instead of racing a half-deleted one.
const STAGED_SUFFIX: &str = ".cargo-seed-gc";

/// `cargo seed gc --root DIR [--min-free-gb N] [--max-idle-days N] [--apply]`
pub(crate) fn gc(root: &Path, min_free: u64, max_idle: Duration, apply: bool) -> io::Result<()> {
    remove_staged(root)?;
    let now = SystemTime::now();
    let mut targets: Vec<(SystemTime, PathBuf)> = Vec::new();
    for target in cargo_targets(root)? {
        let built = newest_build(&target)?.unwrap_or(SystemTime::UNIX_EPOCH);
        targets.push((built, target));
    }
    targets.sort();
    let start_free = free_bytes(root).unwrap_or(u64::MAX);
    let mut free = start_free;
    let mut removed = 0;
    for (built, target) in targets {
        let idle = now.duration_since(built).unwrap_or_default();
        if idle < max_idle && free >= min_free {
            continue;
        }
        if idle < RECENT || BuildLock::try_shared(&target)?.is_none() {
            continue;
        }
        let days = idle.as_secs() / 86_400;
        if !apply {
            println!("{}: built {days} days ago; would remove", target.display());
            removed += 1;
            continue;
        }
        let staged = target.with_file_name(format!(
            "{}{STAGED_SUFFIX}",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        fs::rename(&target, &staged)?;
        fs::remove_dir_all(&staged)?;
        free = free_bytes(root).unwrap_or(free);
        println!(
            "{}: built {days} days ago; removed, {} free",
            target.display(),
            gigabytes(free)
        );
        removed += 1;
    }
    if apply {
        let freed = free.saturating_sub(start_free);
        println!("removed {removed} targets, {} freed", gigabytes(freed));
    } else {
        println!("would remove {removed} targets (--apply removes)");
    }
    Ok(())
}

/// Finishes removals an interrupted run left renamed.
fn remove_staged(root: &Path) -> io::Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            if !entry.file_type().is_ok_and(|kind| kind.is_dir())
                || name == ".git"
                || name == "node_modules"
            {
                continue;
            }
            if name.to_string_lossy().ends_with(STAGED_SUFFIX) {
                fs::remove_dir_all(entry.path())?;
            } else if !entry.path().join("CACHEDIR.TAG").exists()
                && dir.components().count() < root.components().count() + 4
            {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}
