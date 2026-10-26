//! `cargo seed`: new git worktrees start with a warm cargo target, and build
//! output nobody uses goes.

mod build_lock;
mod clone;
mod dedupe;
mod disk;
mod fingerprint;
mod gc;
mod git;
mod mtimes;
mod prune;
mod real_path;
mod target_dir;
mod tree_copy;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "\
cargo seed: new git worktrees start with a warm cargo target

A new worktree gets a copy of the target of the closest worktree of the same
repository: the one whose files differ least. On APFS, btrfs, XFS and ReFS the
copy shares disk blocks with it. Cargo then rebuilds exactly what differs.

usage:
  cargo seed install-hook [REPO]
      Make `git worktree add` copy a target into every new worktree.
  cargo seed clone [TREE] [--from WORKTREE]
      Copy a target into TREE, a worktree that has none.
  cargo seed prune [REPO] [--root DIR] [--idle-minutes N] [--apply]
      Remove from the targets of the repository's worktrees, or of every
      cargo target under DIR, the incremental state no build touched for N
      minutes (default 60) and the builds a newer build of the same unit
      replaced.
  cargo seed dedupe --root DIR [--apply]
      Make byte-identical build files across the targets under DIR share disk
      blocks (APFS, btrfs, XFS).
  cargo seed gc --root DIR [--min-free-gb N] [--max-idle-days N] [--apply]
      Remove whole targets under DIR, least recently built first: every one
      unused for N days (default 3), then more until N GB are free
      (default 100).
  cargo seed maintain --root DIR
      gc, prune and dedupe with their defaults, applied: for a scheduled job.

Targets with a build running or finished recently are left alone. Without
--apply, prune, dedupe and gc only report.
";

/// The options of every command; each reads the ones it takes.
struct Options {
    path: PathBuf,
    from: Option<PathBuf>,
    root: Option<PathBuf>,
    hook: bool,
    apply: bool,
    idle_minutes: u64,
    min_free_gb: u64,
    max_idle_days: u64,
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Cargo runs a subcommand as `cargo-seed seed ...`.
    if args.first().is_some_and(|arg| arg == "seed") {
        args.remove(0);
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cargo-seed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse(rest: &[String]) -> Result<Options, String> {
    let mut options = Options {
        path: PathBuf::from("."),
        from: None,
        root: None,
        hook: false,
        apply: false,
        idle_minutes: 60,
        min_free_gb: 100,
        max_idle_days: 3,
    };
    let mut path = None;
    let mut rest = rest.iter();
    let number = |value: Option<&String>, flag: &str| -> Result<u64, String> {
        value
            .and_then(|value| value.parse().ok())
            .ok_or(format!("{flag} needs a number"))
    };
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--from" => {
                options.from = Some(PathBuf::from(rest.next().ok_or("--from needs a worktree")?))
            }
            "--root" => {
                options.root = Some(PathBuf::from(
                    rest.next().ok_or("--root needs a directory")?,
                ))
            }
            "--hook" => options.hook = true,
            "--apply" => options.apply = true,
            "--idle-minutes" => options.idle_minutes = number(rest.next(), "--idle-minutes")?,
            "--min-free-gb" => options.min_free_gb = number(rest.next(), "--min-free-gb")?,
            "--max-idle-days" => options.max_idle_days = number(rest.next(), "--max-idle-days")?,
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option {flag}\n\n{USAGE}"));
            }
            _ if path.is_none() => path = Some(PathBuf::from(arg)),
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    if let Some(path) = path {
        options.path = path;
    }
    Ok(options)
}

fn run(args: &[String]) -> Result<(), String> {
    let Some((command, rest)) = args.split_first() else {
        print!("{USAGE}");
        return Ok(());
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(());
    }
    let options = parse(rest)?;
    let idle = Duration::from_secs(options.idle_minutes * 60);
    let max_idle = Duration::from_secs(options.max_idle_days * 86_400);
    let min_free = options.min_free_gb * 1_000_000_000;
    let root = || {
        options
            .root
            .clone()
            .ok_or(format!("{command} needs --root DIR"))
    };
    let result = match command.as_str() {
        "clone" => clone::clone(&options.path, options.from.as_deref(), options.hook),
        "install-hook" => clone::install_hook(&options.path),
        "prune" => prune::prune(
            match &options.root {
                Some(root) => prune::Scope::Root(root.clone()),
                None => prune::Scope::Repo(options.path.clone()),
            },
            idle,
            options.apply,
        ),
        "dedupe" => dedupe::dedupe(&root()?, options.apply),
        "gc" => gc::gc(&root()?, min_free, max_idle, options.apply),
        "maintain" => {
            let root = root()?;
            gc::gc(&root, min_free, max_idle, true)
                .and_then(|()| prune::prune(prune::Scope::Root(root.clone()), idle, true))
                .and_then(|()| dedupe::dedupe(&root, true))
        }
        other => return Err(format!("unknown command {other}\n\n{USAGE}")),
    };
    result.map_err(|error| error.to_string())
}
