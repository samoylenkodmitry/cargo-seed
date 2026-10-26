//! `cargo seed`: start every new git worktree with a warm cargo target.

mod fingerprint;
mod git;
mod mtimes;
mod seed;
mod tree_copy;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
cargo seed: start every new git worktree with a warm cargo target

A seed is a locked worktree at the remote's default branch whose target holds
what you build. A new worktree gets a copy-on-write clone of that target (APFS,
btrfs, XFS, ReFS; a plain copy elsewhere), corrected so cargo rebuilds exactly
what differs from the seed.

usage:
  cargo seed refresh [REPO] [--warm COMMAND]...
      Make a seed at the followed commit, warm it, and retire the old one.
      COMMAND defaults to the `seed.warm` values in git config, else
      `cargo build --workspace --all-targets`.
  cargo seed clone [TREE] [--seed SEED] [--hook]
      Clone the seed's target into TREE, a worktree that has none.
  cargo seed install-hook [REPO]
      Make `git worktree add` run `clone` for every new worktree.
  cargo seed status [REPO]

git config:
  seed.warm         a warm command; repeat the key for more
  seed.ref          the commit seeds follow (default: origin/HEAD)
  seed.autoRefresh  false stops the hook from refreshing a seed that fell behind
";

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

fn run(args: &[String]) -> Result<(), String> {
    let Some((command, rest)) = args.split_first() else {
        print!("{USAGE}");
        return Ok(());
    };
    let mut path = None;
    let mut seed_path = None;
    let mut warm = Vec::new();
    let mut from_hook = false;
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--seed" => seed_path = Some(PathBuf::from(rest.next().ok_or("--seed needs a path")?)),
            "--warm" => warm.push(rest.next().ok_or("--warm needs a command")?.clone()),
            "--hook" => from_hook = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option {flag}\n\n{USAGE}"));
            }
            _ if path.is_none() => path = Some(PathBuf::from(arg)),
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let result = match command.as_str() {
        "refresh" => seed::refresh(&path, &warm),
        "clone" => seed::clone(&path, seed_path.as_deref(), from_hook),
        "install-hook" => seed::install_hook(&path),
        "status" => seed::status(&path),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => return Err(format!("unknown command {other}\n\n{USAGE}")),
    };
    result.map_err(|error| error.to_string())
}
