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

/// The resolved path in the form git takes: Windows canonicalizes to `\\?\C:\...`.
fn plain_path(path: &Path) -> PathBuf {
    let resolved = fs::canonicalize(path).unwrap();
    match resolved
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
    {
        Some(plain) => PathBuf::from(plain),
        None => resolved,
    }
}

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
            root: plain_path(&root),
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
            &["config", "seed.minFreeGb", "0"],
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
            &["seed", "clone", "--from", self.seed().to_str().unwrap()],
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
fn the_hook_runs_before_an_existing_hook_can_exit() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    write(
        &seed.join(".git/hooks/post-checkout"),
        "#!/bin/sh\nexit 0\n",
    );
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

/// Gives `path`, a file or a directory, a modification time `minutes` in the past.
fn age(path: &Path, minutes: u64) {
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(minutes * 60);
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::File::options()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap()
    };
    #[cfg(not(windows))]
    let file = fs::File::open(path).unwrap();
    file.set_modified(then).unwrap();
}

#[test]
fn prune_removes_idle_incremental_state_but_not_a_busy_targets() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    let idle_tree = fixture.worktree("idle", "HEAD");
    let busy_tree = fixture.worktree("busy", "HEAD");
    for tree in [&idle_tree, &busy_tree] {
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
}

#[test]
fn a_copy_carries_the_cache_tag_sweeps_recognise_even_when_the_donor_lacks_it() {
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

#[test]
fn dedupe_shares_only_identical_files_and_keeps_their_content_and_times() {
    let root =
        plain_path(&std::env::temp_dir()).join(format!("cargo-seed-dedupe-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let same: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
    let mut other = same.clone();
    other[100_000] ^= 1;
    for (repo, odd) in [("one", &same), ("two", &other)] {
        let target = root.join(repo).join("target");
        write(
            &target.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );
        fs::create_dir_all(target.join("debug/.fingerprint")).unwrap();
        fs::create_dir_all(target.join("debug/deps")).unwrap();
        fs::write(
            target.join("debug/deps/libsame-0123456789abcdef.rlib"),
            &same,
        )
        .unwrap();
        fs::write(target.join("debug/deps/libodd-0123456789abcdef.rlib"), odd).unwrap();
        for path in [
            "debug/deps/libsame-0123456789abcdef.rlib",
            "debug/deps/libodd-0123456789abcdef.rlib",
            "debug/.fingerprint",
        ] {
            age(&target.join(path), 120);
        }
    }
    let modified = |repo: &str, name: &str| {
        fs::metadata(root.join(repo).join("target/debug/deps").join(name))
            .unwrap()
            .modified()
            .unwrap()
    };
    let before = modified("two", "libsame-0123456789abcdef.rlib");
    let tool = |args: &[&str]| {
        let mut tool = command(env!("CARGO_BIN_EXE_cargo-seed"), &root);
        tool.args(args);
        ok(tool)
    };
    let report = tool(&["seed", "dedupe", "--root", root.to_str().unwrap()]);
    assert!(report.contains("in 1 files"), "{report}");
    tool(&[
        "seed",
        "dedupe",
        "--root",
        root.to_str().unwrap(),
        "--apply",
    ]);
    for repo in ["one", "two"] {
        assert_eq!(
            fs::read(
                root.join(repo)
                    .join("target/debug/deps/libsame-0123456789abcdef.rlib")
            )
            .unwrap(),
            same
        );
    }
    assert_eq!(
        fs::read(root.join("two/target/debug/deps/libodd-0123456789abcdef.rlib")).unwrap(),
        other
    );
    assert_eq!(
        modified("two", "libsame-0123456789abcdef.rlib"),
        before,
        "the copy's time changed"
    );
    if cfg!(target_vendor = "apple") {
        let again = tool(&["seed", "dedupe", "--root", root.to_str().unwrap()]);
        assert!(
            again.contains("in 0 files"),
            "an APFS clone was not recognised as shared: {again}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

/// Holds cargo's build lock on `tree`'s target as a running build does.
fn hold_build_lock(tree: &Path) -> fs::File {
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(tree.join("target/debug/.cargo-build-lock"))
        .unwrap();
    lock.lock().unwrap();
    lock
}

#[test]
fn a_new_worktree_copies_the_closest_worktree() {
    let fixture = Fixture::new();
    fixture.seed();
    let branch = fixture.worktree("work", "HEAD");
    fixture.git(&branch, &["switch", "-q", "-c", "work"]);
    write(
        &branch.join("base/src/lib.rs"),
        "pub fn value() -> u32 { 4 }\n",
    );
    fixture.git(&branch, &["commit", "-qam", "work"]);
    fixture.cargo(&branch, &["build", "--quiet"]);
    let tree = fixture.worktree("next", "work");
    let report = fixture.cargo_seed(&tree, &["seed", "clone"]);
    assert!(
        report.contains(&format!("from {}", branch.display())),
        "{report}"
    );
    assert_eq!(fixture.run(&tree), fixture.expected(&tree, "4 2 data-1"));
}

#[test]
fn a_worktree_with_a_build_running_is_not_copied() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    let _build = hold_build_lock(&seed);
    let tree = fixture.worktree("next", "HEAD");
    let report = fixture.cargo_seed(&tree, &["seed", "clone"]);
    assert!(report.contains("no worktree"), "{report}");
    assert!(!tree.join("target").exists());
}

#[test]
fn gc_removes_targets_unused_for_days_and_keeps_recent_and_busy_ones() {
    let fixture = Fixture::new();
    let seed = fixture.seed();
    let old = fixture.seeded("old", "HEAD");
    let recent = fixture.seeded("recent", "HEAD");
    let busy = fixture.seeded("busy", "HEAD");
    for (tree, minutes) in [(&old, 5 * 24 * 60), (&recent, 3 * 60), (&busy, 5 * 24 * 60)] {
        for dir in [
            tree.join("target/debug/.fingerprint"),
            tree.join("target/debug"),
            tree.join("target"),
        ] {
            age(&dir, minutes);
        }
    }
    // Other tools tag their caches the same way, ruff for one.
    let other_cache = fixture.root.join("tool/.ruff_cache");
    write(
        &other_cache.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    age(&other_cache, 5 * 24 * 60);
    let _build = hold_build_lock(&busy);
    let root = fixture.root.to_str().unwrap().to_owned();
    let gc = |apply: bool| {
        let mut args = vec![
            "seed",
            "gc",
            "--root",
            &root,
            "--max-idle-days",
            "3",
            "--min-free-gb",
            "0",
        ];
        if apply {
            args.push("--apply");
        }
        fixture.cargo_seed(&seed, &args)
    };
    gc(false);
    assert!(old.join("target").exists(), "a dry run removed a target");
    gc(true);
    assert!(
        !old.join("target").exists(),
        "a target unused for 5 days stayed"
    );
    assert!(recent.join("target").exists(), "a recent target went");
    assert!(
        busy.join("target").exists(),
        "a target with a build running went"
    );
    assert!(other_cache.exists(), "another tool's cache went");
}
