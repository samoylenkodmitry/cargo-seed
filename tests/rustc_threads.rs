//! `rustc-threads` adds the parallel front end where it pays and nowhere else.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// A workspace whose `app` depends on `base`, built through `rustc-threads`
/// with a `rustc` that logs each call's crate, `-Zthreads` and
/// `RUSTC_BOOTSTRAP`. Returns the log lines.
fn logged_build(name: &str, cargo_args: &[&str]) -> Vec<String> {
    let root =
        std::env::temp_dir().join(format!("rustc-threads-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"base\", \"app\"]\nresolver = \"2\"\n",
    );
    write(
        &root.join("base/Cargo.toml"),
        "[package]\nname = \"base\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &root.join("base/src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    );
    write(
        &root.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nbase = { path = \"../base\" }\n",
    );
    write(
        &root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", base::value()); }\n",
    );
    let log = root.join("calls.log");
    let real_rustc = String::from_utf8(
        Command::new("rustup")
            .args(["which", "rustc"])
            .current_dir(&root)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let logger = root.join("logging-rustc");
    write(
        &logger,
        &format!(
            "#!/bin/sh\nname=\"\"; prev=\"\"; threads=no\nfor arg in \"$@\"; do\n  [ \"$prev\" = --crate-name ] && name=\"$arg\"\n  case \"$arg\" in -Zthreads=*) threads=yes ;; esac\n  prev=\"$arg\"\ndone\n[ -n \"$name\" ] && echo \"$name threads=$threads bootstrap=${{RUSTC_BOOTSTRAP:-}}\" >> {log}\nexec {real_rustc} \"$@\"\n",
            log = log.display()
        ),
    );
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&logger, fs::Permissions::from_mode(0o755)).unwrap();
    let status = Command::new("cargo")
        .args(cargo_args)
        .current_dir(&root)
        .env("RUSTC", &logger)
        .env("RUSTC_WRAPPER", env!("CARGO_BIN_EXE_rustc-threads"))
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTC_BOOTSTRAP")
        .status()
        .unwrap();
    assert!(status.success());
    let lines = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let _ = fs::remove_dir_all(PathBuf::from(&root));
    lines
}

#[test]
fn a_dev_build_runs_the_selected_crate_in_parallel_and_its_dependency_as_given() {
    let calls = logged_build("dev", &["build", "-q", "-p", "app"]);
    assert!(
        calls.contains(&"app threads=yes bootstrap=app".to_owned()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"base threads=no bootstrap=".to_owned()),
        "{calls:?}"
    );
}

#[test]
fn a_release_build_runs_as_given() {
    let calls = logged_build("release", &["build", "-q", "--release", "-p", "app"]);
    assert!(
        calls
            .iter()
            .all(|call| call.contains("threads=no bootstrap=")),
        "{calls:?}"
    );
}
