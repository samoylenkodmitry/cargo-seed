//! A seeded worktree builds what a cold build of the same tree builds.
//!
//! Each test makes a throwaway cargo workspace in a git repository and drives
//! the `cargo-seed` binary on it. The workspace exercises each way a cloned
//! target could go stale: a source a branch changed, a source the seed edited
//! after its build, a directory a build script watches with a file removed
//! from it, a file watched by absolute path, and a crate whose code holds its
//! own path. The checks read the program's output, so a unit that should have
//! rebuilt and did not shows up as a wrong value; the cases that stay fresh
//! check that cargo compiled nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn command(program: &str, dir: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(dir);
    for name in [
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "RUSTC_WRAPPER",
        "CARGO_INCREMENTAL",
        "CARGO_TERM_QUIET",
        "CARGO_TERM_VERBOSE",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(name);
    }
    // The checks read cargo's "Compiling" lines.
    command.env("CARGO_TERM_COLOR", "never");
    // Nothing reaches the machine's own git setup: a global core.hooksPath
    // folder serves every repository on it.
    command.env(
        "GIT_CONFIG_GLOBAL",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    // The hook finds the binary under test first.
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_cargo-seed"))
        .parent()
        .unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin_dir.to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    command.env("PATH", path);
    command
}

fn ok(mut command: Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

impl Fixture {
    /// A seed repository with the workspace committed and built.
    fn new() -> Fixture {
        let name = format!(
            "cargo-seed-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let fixture = Fixture {
            root: fs::canonicalize(&root).unwrap(),
        };
        let seed = fixture.seed();
        write(
            &seed.join("Cargo.toml"),
            "[workspace]\nmembers = [\"base\", \"paths\", \"watcher\", \"app\"]\nresolver = \"2\"\n",
        );
        for name in ["base", "paths", "watcher"] {
            write(
                &seed.join(name).join("Cargo.toml"),
                &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            );
        }
        write(
            &seed.join("base/src/lib.rs"),
            "pub fn value() -> u32 { 1 }\n",
        );
        write(
            &seed.join("paths/src/lib.rs"),
            "pub const DIR: &str = env!(\"CARGO_MANIFEST_DIR\");\n",
        );
        write(
            &seed.join("watcher/build.rs"),
            r#"use std::{env, fs, path::Path};
fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let data = Path::new(&manifest).join("../data.txt").canonicalize().unwrap();
    println!("cargo::rerun-if-changed=assets");
    println!("cargo::rerun-if-changed={}", data.display());
    let assets = fs::read_dir("assets").unwrap().count();
    let data = fs::read_to_string(data).unwrap();
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("gen.rs");
    fs::write(out, format!("pub const ASSETS: usize = {assets};\npub const DATA: &str = {:?};\n", data.trim())).unwrap();
}
"#,
        );
        write(
            &seed.join("watcher/src/lib.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n",
        );
        write(&seed.join("watcher/assets/one.txt"), "one\n");
        write(&seed.join("watcher/assets/two.txt"), "two\n");
        write(&seed.join("data.txt"), "data-1\n");
        write(
            &seed.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nbase = { path = \"../base\" }\npaths = { path = \"../paths\" }\nwatcher = { path = \"../watcher\" }\n",
        );
        write(
            &seed.join("app/src/main.rs"),
            "fn main() {\n    println!(\"{} {} {} {}\", base::value(), watcher::ASSETS, watcher::DATA, paths::DIR);\n}\n",
        );
        write(&seed.join(".gitignore"), "target\n");
        ok({
            let mut c = command("cargo", &seed);
            c.args(["generate-lockfile", "--quiet"]);
            c
        });
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            &["add", "-A"],
            &["commit", "-qm", "seed"],
        ] {
            fixture.git(&seed, args);
        }
        fixture.cargo(&seed, &["build", "--quiet"]);
        fixture
    }

    fn seed(&self) -> PathBuf {
        self.root.join("seed")
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut git = command("git", dir);
        git.args(args);
        ok(git)
    }

    fn cargo(&self, dir: &Path, args: &[&str]) -> String {
        let mut cargo = command("cargo", dir);
        cargo.args(args);
        ok(cargo)
    }

    fn cargo_seed(&self, dir: &Path, args: &[&str]) -> String {
        let mut tool = command(env!("CARGO_BIN_EXE_cargo-seed"), dir);
        tool.args(args);
        ok(tool)
    }

    /// A worktree of the seed repository at `commit`, with no target.
    fn worktree(&self, name: &str, commit: &str) -> PathBuf {
        let tree = self.root.join(name);
        self.git(
            &self.seed(),
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "-q",
                "--detach",
                tree.to_str().unwrap(),
                commit,
            ],
        );
        tree
    }

    fn seeded(&self, name: &str, commit: &str) -> PathBuf {
        let tree = self.worktree(name, commit);
        self.cargo_seed(
            &tree,
            &["seed", "clone", "--seed", self.seed().to_str().unwrap()],
        );
        tree
    }

    fn run(&self, dir: &Path) -> String {
        self.cargo(dir, &["run", "--quiet", "-p", "app"])
    }

    /// The packages the next build compiles, sorted.
    fn compiled(&self, dir: &Path) -> Vec<String> {
        let mut cargo = command("cargo", dir);
        cargo.args(["build", "-p", "app"]);
        let output = cargo.output().unwrap();
        assert!(output.status.success());
        let mut names: Vec<String> = String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("Compiling "))
            .map(|rest| {
                rest.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn expected(&self, tree: &Path, values: &str) -> String {
        format!("{values} {}", tree.join("paths").display())
    }
}

#[test]
fn a_tree_at_the_seed_commit_rebuilds_only_units_naming_its_path() {
    let fixture = Fixture::new();
    let tree = fixture.seeded("same", "HEAD");
    assert_eq!(fixture.compiled(&tree), ["app", "paths", "watcher"]);
    assert_eq!(fixture.run(&tree), fixture.expected(&tree, "1 2 data-1"));
}

#[test]
fn a_branch_that_changed_a_source_a_watched_directory_and_a_watched_file_builds_the_branch() {
    let fixture = Fixture::new();
    let branch = fixture.worktree("branch", "HEAD");
    fixture.git(&branch, &["switch", "-q", "-c", "work"]);
    write(
        &branch.join("base/src/lib.rs"),
        "pub fn value() -> u32 { 2 }\n",
    );
    fixture.git(&branch, &["rm", "-q", "watcher/assets/two.txt"]);
    write(&branch.join("data.txt"), "data-2\n");
    fixture.git(&branch, &["commit", "-qam", "change"]);
    fixture.git(
        &fixture.seed(),
        &["worktree", "remove", branch.to_str().unwrap()],
    );
    let tree = fixture.seeded("changed", "work");
    assert_eq!(fixture.run(&tree), fixture.expected(&tree, "2 1 data-2"));
}

#[test]
fn a_seed_edited_after_its_build_seeds_the_committed_source() {
    let fixture = Fixture::new();
    write(
        &fixture.seed().join("base/src/lib.rs"),
        "pub fn value() -> u32 { 7 }\n",
    );
    fixture.cargo(&fixture.seed(), &["build", "--quiet"]);
    let tree = fixture.seeded("edited", "HEAD");
    assert_eq!(fixture.run(&tree), fixture.expected(&tree, "1 2 data-1"));
}

#[test]
fn edits_after_seeding_rebuild() {
    let fixture = Fixture::new();
    let tree = fixture.seeded("later", "HEAD");
    fixture.run(&tree);
    write(
        &tree.join("base/src/lib.rs"),
        "pub fn value() -> u32 { 3 }\n",
    );
    write(&tree.join("watcher/assets/three.txt"), "three\n");
    write(&tree.join("data.txt"), "data-3\n");
    assert_eq!(fixture.run(&tree), fixture.expected(&tree, "3 3 data-3"));
}

#[test]
fn the_hook_seeds_new_worktrees_and_leaves_branch_switches_alone() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    fixture.git(&seed, &["config", "seed.path", seed.to_str().unwrap()]);
    fixture.cargo_seed(&seed, &["seed", "install-hook"]);
    let tree = fixture.root.join("hooked");
    fixture.git(
        &seed,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            tree.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(tree.join("target").is_dir(), "the hook seeded nothing");
    assert_eq!(fixture.run(&tree), fixture.expected(&tree, "1 2 data-1"));
    fixture.git(&tree, &["switch", "-q", "-c", "elsewhere"]);
    assert_eq!(fixture.compiled(&tree), Vec::<String>::new());
}

#[test]
fn refresh_makes_a_warm_seed_at_the_remote_and_retires_the_old_one() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    let origin = fixture.root.join("origin.git");
    fixture.git(
        &fixture.root,
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    fixture.git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    fixture.git(&seed, &["push", "-q", "origin", "HEAD:main"]);
    fixture.git(&seed, &["config", "seed.ref", "origin/main"]);
    let refresh =
        || fixture.cargo_seed(&seed, &["seed", "refresh", "--warm", "cargo build -p app"]);

    refresh();
    let first = PathBuf::from(fixture.git(&seed, &["config", "--get", "seed.path"]));
    assert_eq!(
        fixture.git(&first, &["rev-parse", "HEAD"]),
        fixture.git(&seed, &["rev-parse", "origin/main"])
    );
    let locks = fixture.git(&seed, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        locks
            .lines()
            .filter(|line| *line == "locked cargo seed")
            .count(),
        1
    );
    assert_eq!(
        fixture.compiled(&first),
        Vec::<String>::new(),
        "the seed is not warm"
    );

    let next = fixture.worktree("next", "origin/main");
    fixture.git(&next, &["switch", "-q", "-c", "next"]);
    write(
        &next.join("base/src/lib.rs"),
        "pub fn value() -> u32 { 5 }\n",
    );
    fixture.git(&next, &["commit", "-qam", "next"]);
    fixture.git(&next, &["push", "-q", "origin", "HEAD:main"]);

    refresh();
    let second = PathBuf::from(fixture.git(&seed, &["config", "--get", "seed.path"]));
    assert_eq!(
        fixture.git(&second, &["rev-parse", "HEAD"]),
        fixture.git(&next, &["rev-parse", "HEAD"])
    );
    assert!(!first.exists(), "the old seed is still there");
    assert!(fixture.run(&second).starts_with("5 "));
}

#[test]
fn the_hook_runs_before_an_existing_hook_can_exit() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    write(
        &seed.join(".git/hooks/post-checkout"),
        "#!/bin/sh\nexit 0\n",
    );
    fixture.git(&seed, &["config", "seed.path", seed.to_str().unwrap()]);
    fixture.cargo_seed(&seed, &["seed", "install-hook"]);
    let tree = fixture.root.join("hooked");
    fixture.git(
        &seed,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            tree.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(
        tree.join("target").is_dir(),
        "the existing exit skipped the seeding"
    );
}

#[test]
fn install_hook_leaves_a_hooks_path_folder_alone() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    let shared = fixture.root.join("shared-hooks");
    write(&shared.join("post-checkout"), "#!/bin/sh\nexit 0\n");
    fixture.git(
        &seed,
        &["config", "core.hooksPath", shared.to_str().unwrap()],
    );
    let printed = fixture.cargo_seed(&seed, &["seed", "install-hook"]);
    assert_eq!(
        fs::read_to_string(shared.join("post-checkout")).unwrap(),
        "#!/bin/sh\nexit 0\n"
    );
    assert!(
        printed.contains("core.hooksPath"),
        "the user is not told where the line goes: {printed}"
    );
    assert!(
        fs::read_to_string(seed.join(".git/hooks/post-checkout"))
            .unwrap()
            .contains("cargo-seed seed clone")
    );
}

/// Gives `path` a modification time `minutes` in the past.
fn age(path: &Path, minutes: u64) {
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(minutes * 60);
    fs::File::open(path).unwrap().set_modified(then).unwrap();
}

#[test]
fn prune_removes_idle_incremental_state_but_not_a_seeds_or_a_busy_targets() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    let idle_tree = fixture.worktree("idle", "HEAD");
    let busy_tree = fixture.worktree("busy", "HEAD");
    let locked = fixture.worktree("locked-seed", "HEAD");
    fixture.git(
        &seed,
        &[
            "worktree",
            "lock",
            "--reason",
            "cargo seed",
            locked.to_str().unwrap(),
        ],
    );
    for tree in [&idle_tree, &busy_tree, &locked] {
        for (name, minutes) in [("idle_crate-1", 600), ("recent_crate-2", 5)] {
            let dir = tree.join("target/debug/incremental").join(name);
            write(&dir.join("s-session/query-cache.bin"), "state");
            age(&dir, minutes);
        }
        fs::create_dir_all(tree.join("target/debug/.fingerprint")).unwrap();
        let built = if tree == &busy_tree { 1 } else { 600 };
        age(&tree.join("target/debug/.fingerprint"), built);
        age(&tree.join("target/debug"), built);
    }
    let idle = |tree: &Path| tree.join("target/debug/incremental/idle_crate-1").exists();

    fixture.cargo_seed(&seed, &["seed", "prune"]);
    assert!(idle(&idle_tree), "a dry run removed state");

    fixture.cargo_seed(&seed, &["seed", "prune", "--apply"]);
    assert!(!idle(&idle_tree), "idle state stayed");
    assert!(
        idle_tree
            .join("target/debug/incremental/recent_crate-2")
            .exists(),
        "recent state went"
    );
    assert!(
        idle(&busy_tree),
        "a target with a build in flight lost state"
    );
    assert!(idle(&locked), "a seed lost state");
}

#[test]
fn a_clone_carries_the_cache_tag_sweeps_recognise_even_when_the_seed_lacks_it() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.seed().join("target/CACHEDIR.TAG")).unwrap();
    let tree = fixture.seeded("tagged", "HEAD");
    let tag = fs::read_to_string(tree.join("target/CACHEDIR.TAG")).unwrap();
    assert!(tag.starts_with("Signature: 8a477f597d28d172789f06886806bc55"));
}

#[test]
fn prune_removes_builds_a_newer_build_replaced_and_keeps_the_target_fresh() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    let fingerprints = seed.join("target/debug/.fingerprint");
    let units = |prefix: &str| {
        fs::read_dir(&fingerprints)
            .unwrap()
            .filter(|unit| {
                unit.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .count()
    };
    // The workspace was built two hours ago; then `base` gets a new version,
    // which builds `base` and its dependents again under new hashes.
    for unit in fs::read_dir(&fingerprints).unwrap() {
        age(&unit.unwrap().path(), 120);
    }
    write(
        &seed.join("base/Cargo.toml"),
        "[package]\nname = \"base\"\nversion = \"0.2.0\"\nedition = \"2021\"\n",
    );
    fixture.cargo(&seed, &["build", "--quiet"]);
    assert_eq!(units("base-"), 2);
    for dir in [seed.join("target/debug"), fingerprints.clone()] {
        age(&dir, 20);
    }

    fixture.cargo_seed(
        &seed,
        &["seed", "prune", "--root", seed.to_str().unwrap(), "--apply"],
    );
    assert_eq!(units("base-"), 1, "the replaced build of base stayed");
    assert_eq!(
        fixture.compiled(&seed),
        Vec::<String>::new(),
        "a live build went"
    );
    assert_eq!(fixture.run(&seed), fixture.expected(&seed, "1 2 data-1"));
}
