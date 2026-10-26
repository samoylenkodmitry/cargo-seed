use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime};

use crate::fingerprint::move_fingerprints;
use crate::git::Checkout;
use crate::mtimes::copy_mtimes;
use crate::target_dir::incremental_crate_dirs;
use crate::tree_copy::{copy_tree, set_times};

/// Git config keys. `seed.path` is written by `refresh`; the others are the user's.
const PATH_KEY: &str = "seed.path";
const WARM_KEY: &str = "seed.warm";
const REF_KEY: &str = "seed.ref";
const AUTO_REFRESH_KEY: &str = "seed.autoRefresh";
/// The reason a seed worktree is locked with, which marks it as one.
pub(crate) const LOCK_REASON: &str = "cargo seed";
/// Built into a seed when `seed.warm` names nothing.
const DEFAULT_WARM: &str = "cargo build --workspace --all-targets";

/// Where seeds keep their locks and logs: the repository's git directory.
fn state_dir(repo: &Checkout) -> io::Result<PathBuf> {
    let dir = repo.common_dir()?.join("cargo-seed");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn lock_file(repo: &Checkout, seed: &Path) -> io::Result<File> {
    let name = seed.file_name().unwrap_or_default().to_string_lossy();
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state_dir(repo)?.join(format!("{name}.lock")))
}

/// The seed this repository's new worktrees start from.
fn configured_seed(repo: &Checkout) -> Option<PathBuf> {
    repo.config(PATH_KEY)
        .and_then(|path| fs::canonicalize(path).ok())
}

/// The commit seeds follow: `seed.ref`, else the remote's default branch.
fn followed_ref(repo: &Checkout) -> String {
    repo.config(REF_KEY)
        .or_else(|| repo.try_text(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]))
        .unwrap_or_else(|| "origin/main".to_owned())
}

/// Clones `seed`'s target into `tree` and makes it correct for `tree`.
fn seed_tree(seed: &Path, tree: &Path) -> io::Result<()> {
    let target = tree.join("target");
    copy_tree(&seed.join("target"), &target)?;
    mark_incremental_current(&target)?;
    let times = copy_mtimes(seed, tree)?;
    let moved = move_fingerprints(&target, seed, tree)?;
    println!(
        "cargo-seed: {} from {}: {times} file times copied, {} records moved, {} units to rebuild",
        target.display(),
        seed.display(),
        moved.records,
        moved.rebuilds
    );
    Ok(())
}

/// Sweeps that prune incremental state by age should see a fresh clone's as
/// current; rustc reads its session names, not these times.
fn mark_incremental_current(target: &Path) -> io::Result<()> {
    let now = SystemTime::now();
    for crate_dir in incremental_crate_dirs(target)? {
        set_times(&crate_dir, now, now)?;
    }
    Ok(())
}

/// `cargo seed clone [TREE] [--seed SEED] [--hook]`
pub(crate) fn clone(tree: &Path, seed: Option<&Path>, from_hook: bool) -> io::Result<()> {
    let tree_git = Checkout::new(tree);
    let tree = fs::canonicalize(tree_git.top()?)?;
    if tree.join("target").exists() {
        if !from_hook {
            println!("cargo-seed: {} already has a target", tree.display());
        }
        return Ok(());
    }
    let seed = match seed {
        Some(seed) => fs::canonicalize(seed)?,
        None => match configured_seed(&tree_git) {
            Some(seed) => seed,
            None if from_hook => return Ok(()),
            None => {
                return Err(io::Error::other(format!(
                    "no {PATH_KEY}; run `cargo seed refresh` first"
                )));
            }
        },
    };
    {
        let lock = lock_file(&tree_git, &seed)?;
        lock.lock_shared()?;
        if !seed.join("target").is_dir() {
            println!(
                "cargo-seed: seed {} has no target; {} builds from nothing",
                seed.display(),
                tree.display()
            );
            return Ok(());
        }
        seed_tree(&seed, &tree)?;
    }
    let auto_refresh = tree_git
        .config(AUTO_REFRESH_KEY)
        .is_none_or(|value| value != "false");
    if from_hook && auto_refresh {
        let followed =
            tree_git.try_text(["rev-parse", "--verify", "--quiet", &followed_ref(&tree_git)]);
        if followed
            .is_some_and(|commit| Checkout::new(&seed).head().is_ok_and(|head| head != commit))
        {
            start_background_refresh(&tree_git)?;
        }
    }
    Ok(())
}

fn start_background_refresh(repo: &Checkout) -> io::Result<()> {
    let log_path = state_dir(repo)?.join("refresh.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["seed", "refresh"])
        .arg(repo.top()?)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detach(&mut command);
    command.spawn()?;
    println!(
        "cargo-seed: refreshing the seed in the background, log: {}",
        log_path.display()
    );
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

fn seed_worktrees(repo: &Checkout) -> io::Result<Vec<PathBuf>> {
    Ok(repo
        .worktrees()?
        .into_iter()
        .filter(|worktree| worktree.lock_reason.as_deref() == Some(LOCK_REASON))
        .map(|worktree| worktree.path)
        .collect())
}

/// `cargo seed refresh [REPO] [--warm COMMAND]...`
pub(crate) fn refresh(repo: &Path, warm: &[String]) -> io::Result<()> {
    let repo = Checkout::new(repo);
    let state = state_dir(&repo)?;
    let refresh_lock = lock_file(&repo, Path::new("refresh"))?;
    if refresh_lock.try_lock().is_err() {
        println!("cargo-seed: a refresh is already running");
        return Ok(());
    }
    let followed = followed_ref(&repo);
    if let Some((remote, branch)) = followed.split_once('/') {
        repo.run(["fetch", "--quiet", remote, branch])?;
    }
    let commit = repo.text(["rev-parse", "--verify", &followed])?;
    let current = configured_seed(&repo);
    if let Some(current) = current.as_deref().filter(|current| {
        Checkout::new(current)
            .head()
            .is_ok_and(|head| head == commit)
    }) {
        println!("cargo-seed: {} is at {followed} already", current.display());
        return retire_old_seeds(&repo, current);
    }
    let primary = repo
        .common_dir()?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let name = format!(
        "{}-seed-{}",
        primary.file_name().unwrap_or_default().to_string_lossy(),
        &commit[..10]
    );
    let seed = primary.parent().unwrap_or(&primary).join(name);
    if !seed_worktrees(&repo)?.contains(&seed) {
        // Hooks stay off: this seeds the worktree itself, and the hook would
        // start another refresh.
        let path = seed.to_string_lossy();
        repo.run([
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            "--quiet",
            "--detach",
            &path,
            &commit,
        ])?;
        repo.run(["worktree", "lock", "--reason", LOCK_REASON, &path])?;
        if let Some(current) = current
            .as_deref()
            .filter(|current| current.join("target").is_dir())
        {
            let lock = lock_file(&repo, current)?;
            lock.lock_shared()?;
            seed_tree(current, &seed)?;
        }
    }
    let seed = fs::canonicalize(&seed)?;
    lower_priority();
    let commands = if warm.is_empty() {
        repo.config_all(WARM_KEY)
    } else {
        warm.to_vec()
    };
    let commands = if commands.is_empty() {
        vec![DEFAULT_WARM.to_owned()]
    } else {
        commands
    };
    let log = File::create(state.join("warm.log"))?;
    for command in commands {
        let started = Instant::now();
        let status = shell(&command)
            .current_dir(&seed)
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .status()?;
        let result = if status.success() {
            "ok".to_owned()
        } else {
            format!("{status}")
        };
        println!(
            "cargo-seed: warm `{command}`: {result} in {} s",
            started.elapsed().as_secs()
        );
    }
    repo.run(["config", PATH_KEY, &seed.to_string_lossy()])?;
    println!(
        "cargo-seed: new worktrees now start from {}",
        seed.display()
    );
    retire_old_seeds(&repo, &seed)
}

/// A warm command as the user wrote it, run by the platform's shell, with
/// the environment a plain build in a worktree sees.
fn shell(command: &str) -> Command {
    let mut shell = if cfg!(windows) {
        Command::new("cmd")
    } else {
        Command::new("sh")
    };
    shell
        .arg(if cfg!(windows) { "/C" } else { "-c" })
        .arg(command);
    for name in [
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "RUSTC_WRAPPER",
        "CARGO_INCREMENTAL",
    ] {
        shell.env_remove(name);
    }
    shell
}

#[cfg(unix)]
fn lower_priority() {
    // SAFETY: nice only changes this process's scheduling priority.
    unsafe {
        libc::nice(10);
    }
}

#[cfg(windows)]
fn lower_priority() {}

/// Removes every other seed that no clone reads now and that holds nothing
/// but build output; a later refresh retries the rest.
fn retire_old_seeds(repo: &Checkout, current: &Path) -> io::Result<()> {
    for old in seed_worktrees(repo)? {
        if fs::canonicalize(&old).is_ok_and(|old| old == current) {
            continue;
        }
        let lock = lock_file(repo, &old)?;
        if lock.try_lock().is_err() {
            println!(
                "cargo-seed: {} is being cloned; a later refresh removes it",
                old.display()
            );
            continue;
        }
        if Checkout::new(&old).has_work()? {
            println!("cargo-seed: keeping {}: it holds changes", old.display());
            continue;
        }
        let path = old.to_string_lossy();
        repo.run(["worktree", "unlock", &path])?;
        repo.run(["worktree", "remove", "--force", &path])?;
        println!("cargo-seed: removed the old seed {}", old.display());
    }
    Ok(())
}

/// `cargo seed status`
pub(crate) fn status(repo: &Path) -> io::Result<()> {
    let repo = Checkout::new(repo);
    let followed = followed_ref(&repo);
    match configured_seed(&repo) {
        None => println!("no seed; `cargo seed refresh` makes one"),
        Some(seed) => {
            let head = Checkout::new(&seed).head()?;
            let behind = repo
                .try_text(["rev-list", "--count", &format!("{head}..{followed}")])
                .unwrap_or_default();
            println!(
                "seed: {}\nat: {head}\nbehind {followed}: {behind} commits",
                seed.display()
            );
        }
    }
    Ok(())
}

const HOOK_LINE: &str = r#"[ "$1" = 0000000000000000000000000000000000000000 ] && command -v cargo-seed >/dev/null 2>&1 && cargo-seed seed clone --hook"#;

/// `cargo seed install-hook`: makes `git worktree add` seed new worktrees.
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
        println!("cargo-seed: {} already seeds new worktrees", hook.display());
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
        println!("cargo-seed: {} now seeds new worktrees", hook.display());
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
