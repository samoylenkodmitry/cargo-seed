//! A new worktree's target, copied from the closest worktree of the same
//! repository that has one.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::build_lock::BuildLock;
use crate::fingerprint::move_fingerprints;
use crate::git::Checkout;
use crate::mtimes::copy_mtimes;
use crate::real_path::real_path;
use crate::target_dir::{incremental_crate_dirs, newest_build};
use crate::tree_copy::{copy_tree, set_times};

/// How often new worktrees start a prune of the repository's worktrees.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// The tag cargo writes into a target it creates (https://bford.info/cachedir/).
const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
# This file is a cache directory tag created by cargo.\n\
# For information about cache directory tags see https://bford.info/cachedir/\n";

/// Where cargo-seed keeps its stamps and logs: the repository's git directory.
fn state_dir(repo: &Checkout) -> io::Result<PathBuf> {
    let dir = repo.common_dir()?.join("cargo-seed");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The repository's main checkout: the parent of the shared git directory,
/// in the path form the platform's records use.
fn primary_checkout(repo: &Checkout) -> io::Result<PathBuf> {
    let common = real_path(&repo.common_dir()?)?;
    Ok(common.parent().map(Path::to_path_buf).unwrap_or(common))
}

/// A worktree whose target a new worktree can copy.
struct Donor {
    path: PathBuf,
    /// Committed files that differ from the new worktree's, plus the donor's
    /// own edits: each is a reason for cargo to rebuild something.
    differences: usize,
    built: SystemTime,
}

/// The worktree of `tree`'s repository closest to `tree` that has a target
/// and no build running: fewest differing files, then the newest build.
fn closest_donor(tree_git: &Checkout, tree: &Path) -> io::Result<Option<PathBuf>> {
    let head = tree_git.head()?;
    let mut donors = Vec::new();
    for worktree in tree_git.worktrees()? {
        let Ok(path) = real_path(&worktree.path) else {
            continue;
        };
        let target = path.join("target");
        if path == tree || !target.is_dir() {
            continue;
        }
        if BuildLock::try_shared(&target)?.is_none() {
            continue;
        }
        let donor_git = Checkout::new(&path);
        let Ok(donor_head) = donor_git.head() else {
            continue;
        };
        let changed = tree_git.bytes(["diff", "--name-only", "-z", &donor_head, &head])?;
        let edited = donor_git.bytes(["diff", "--name-only", "-z", "HEAD", "--"])?;
        let count = |listing: &[u8]| {
            listing
                .split(|&byte| byte == 0)
                .filter(|name| !name.is_empty())
                .count()
        };
        let built = newest_build(&target)?.unwrap_or(SystemTime::UNIX_EPOCH);
        donors.push(Donor {
            path,
            differences: count(&changed) + count(&edited),
            built,
        });
    }
    donors.sort_by(|a, b| {
        a.differences
            .cmp(&b.differences)
            .then(b.built.cmp(&a.built))
    });
    Ok(donors.into_iter().next().map(|donor| donor.path))
}

/// Copies `donor`'s target into `tree` and makes it correct for `tree`.
fn copy_target(donor: &Path, tree: &Path) -> io::Result<()> {
    let target = tree.join("target");
    {
        // No build starts in the donor while its target is copied.
        let Some(_lock) = BuildLock::try_shared(&donor.join("target"))? else {
            println!(
                "cargo-seed: a build started in {}; {} builds from nothing",
                donor.display(),
                tree.display()
            );
            return Ok(());
        };
        copy_tree(&donor.join("target"), &target)?;
    }
    // Cargo tags a target only when it creates the directory itself; sweeps
    // recognise build output by the tag, so a copy always carries one.
    let tag = target.join("CACHEDIR.TAG");
    if !tag.exists() {
        fs::write(&tag, CACHEDIR_TAG)?;
    }
    mark_incremental_current(&target)?;
    let times = copy_mtimes(donor, tree)?;
    let moved = move_fingerprints(&target, donor, tree)?;
    println!(
        "cargo-seed: {} from {}: {times} file times copied, {} records moved, {} units to rebuild",
        target.display(),
        donor.display(),
        moved.records,
        moved.rebuilds
    );
    Ok(())
}

/// Sweeps that prune incremental state by age should see a fresh copy's as
/// current; rustc reads its session names, not these times.
fn mark_incremental_current(target: &Path) -> io::Result<()> {
    let now = SystemTime::now();
    for crate_dir in incremental_crate_dirs(target)? {
        set_times(&crate_dir, now, now)?;
    }
    Ok(())
}

/// `cargo seed clone [TREE] [--from WORKTREE] [--hook]`
pub(crate) fn clone(tree: &Path, from: Option<&Path>, from_hook: bool) -> io::Result<()> {
    let tree_git = Checkout::new(tree);
    let tree = real_path(&tree_git.top()?)?;
    if tree.join("target").exists() {
        if !from_hook {
            println!("cargo-seed: {} already has a target", tree.display());
        }
        return Ok(());
    }
    let donor = match from {
        Some(from) => Some(real_path(from)?),
        None => closest_donor(&tree_git, &tree)?,
    };
    match donor {
        Some(donor) => copy_target(&donor, &tree)?,
        None if from_hook => {}
        None => println!("cargo-seed: no worktree of this repository has a target to copy"),
    }
    if from_hook && prune_due(&tree_git)? {
        start_background_prune(&tree_git)?;
    }
    Ok(())
}

/// Whether the repository's worktrees went an hour without a prune; marks
/// one as started when they did. New worktrees are when targets grow.
fn prune_due(repo: &Checkout) -> io::Result<bool> {
    let stamp = state_dir(repo)?.join("last-prune");
    let last = fs::metadata(&stamp).and_then(|meta| meta.modified()).ok();
    if last.is_some_and(|last| last.elapsed().unwrap_or_default() < PRUNE_INTERVAL) {
        return Ok(false);
    }
    fs::write(&stamp, "")?;
    Ok(true)
}

/// Runs `cargo seed prune <primary checkout> --apply` detached, from the
/// primary checkout: a process that names or stands in the new worktree would
/// keep it busy for tools that remove worktrees.
fn start_background_prune(repo: &Checkout) -> io::Result<()> {
    let log_path = state_dir(repo)?.join("prune.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let primary = primary_checkout(repo)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["seed", "prune"])
        .arg(&primary)
        .arg("--apply")
        .current_dir(&primary)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detach(&mut command);
    command.spawn()?;
    Ok(())
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

const HOOK_LINE: &str = r#"[ "$1" = 0000000000000000000000000000000000000000 ] && command -v cargo-seed >/dev/null 2>&1 && cargo-seed seed clone --hook"#;

/// `cargo seed install-hook`: makes `git worktree add` copy a target.
///
/// Writes only this repository's own `hooks/post-checkout`, our line first so
/// an `exit` or `exec` later in an existing hook cannot skip it. A
/// `core.hooksPath` folder can serve every repository on the machine, so
/// when one is set this names the line to add there and touches nothing in it.
pub(crate) fn install_hook(repo: &Path) -> io::Result<()> {
    let repo = Checkout::new(repo);
    let hook = repo.common_dir()?.join("hooks").join("post-checkout");
    let existing = fs::read_to_string(&hook).unwrap_or_default();
    let line = format!("{HOOK_LINE} || true\n");
    if existing.contains(HOOK_LINE) {
        println!(
            "cargo-seed: {} already copies targets into new worktrees",
            hook.display()
        );
    } else {
        let script = match existing.split_once('\n') {
            Some((shebang, rest)) if shebang.starts_with("#!") => {
                format!("{shebang}\n{line}{rest}")
            }
            _ if existing.is_empty() => format!("#!/bin/sh\n{line}"),
            _ => format!("{line}{existing}"),
        };
        fs::create_dir_all(hook.parent().unwrap_or(Path::new(".")))?;
        fs::write(&hook, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
        }
        println!(
            "cargo-seed: {} now copies targets into new worktrees",
            hook.display()
        );
    }
    if let Some(hooks_path) = repo.config("core.hooksPath") {
        println!(
            "cargo-seed: git runs hooks from {hooks_path} (core.hooksPath), so {} runs only if \
             that folder hands post-checkout on to it. Otherwise add this line to its post-checkout:\n{line}",
            hook.display()
        );
    }
    Ok(())
}
