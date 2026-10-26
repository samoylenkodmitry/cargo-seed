use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::git::Checkout;
use crate::seed::LOCK_REASON;
use crate::target_dir::{incremental_crate_dirs, profile_dirs};

/// A target written within this long has a build in flight.
const BUSY: Duration = Duration::from_secs(15 * 60);

/// `cargo seed prune [REPO] [--idle-minutes N] [--apply]`
///
/// Incremental state is most of a busy worktree's target, and most of it
/// belongs to crate builds the worktree stopped repeating: each feature set,
/// profile and test target of a crate keeps its own. rustc renews a crate's
/// incremental directory with every session, so its mtime is the crate's last
/// build; one idle longer than `idle` goes, and rustc rebuilds that crate
/// without reuse the next time it changes. Seeds keep theirs, and so does a
/// target with a build in flight.
pub(crate) fn prune(repo: &Path, idle: Duration, apply: bool) -> io::Result<()> {
    let now = SystemTime::now();
    let mut total_bytes = 0;
    let mut total_dirs = 0;
    for worktree in Checkout::new(repo).worktrees()? {
        let target = worktree.path.join("target");
        if worktree.lock_reason.as_deref() == Some(LOCK_REASON) || !target.is_dir() {
            continue;
        }
        if newest_build(&target)?.is_some_and(|built| age(now, built) < BUSY) {
            println!("{}: a build is in flight; kept", target.display());
            continue;
        }
        let (mut bytes, mut dirs) = (0, 0);
        for crate_dir in incremental_crate_dirs(&target)? {
            if age(now, fs::metadata(&crate_dir)?.modified()?) < idle {
                continue;
            }
            bytes += size(&crate_dir)?;
            dirs += 1;
            if apply {
                fs::remove_dir_all(&crate_dir)?;
            }
        }
        if dirs > 0 {
            let verb = if apply { "removed" } else { "would remove" };
            println!(
                "{}: {verb} {} of idle incremental state in {dirs} crates",
                target.display(),
                gigabytes(bytes)
            );
        }
        total_bytes += bytes;
        total_dirs += dirs;
    }
    let verb = if apply {
        "removed"
    } else {
        "would remove (--apply removes)"
    };
    println!("{verb} {} in {total_dirs} crates", gigabytes(total_bytes));
    Ok(())
}

fn age(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
}

fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// The newest mtime of the target's profile directories and their
/// `.fingerprint`, which cargo rewrites whenever it builds anything.
fn newest_build(target: &Path) -> io::Result<Option<SystemTime>> {
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

/// Bytes the files under `dir` hold.
fn size(dir: &Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        total += if meta.is_dir() {
            size(&entry.path())?
        } else {
            meta.len()
        };
    }
    Ok(total)
}
