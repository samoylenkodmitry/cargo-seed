use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::git::Checkout;
use crate::real_path::real_path;
use crate::seed::LOCK_REASON;
use crate::target_dir::{cargo_targets, incremental_crate_dirs, profile_dirs};

/// A target written within this long has a build in flight.
const BUSY: Duration = Duration::from_secs(15 * 60);
/// Builds of one unit made this close together belong to the same build
/// session, where cargo can need two of them at once.
const SAME_SESSION: Duration = Duration::from_secs(60 * 60);

/// Which targets a prune looks at.
pub(crate) enum Scope {
    /// The targets of one repository's worktrees.
    Repo(PathBuf),
    /// Every cargo target under a directory.
    Root(PathBuf),
}

/// What a sweep removed, or would remove.
#[derive(Default)]
pub(crate) struct Swept {
    incremental_bytes: u64,
    incremental_dirs: usize,
    superseded_bytes: u64,
    superseded_units: usize,
}

impl Swept {
    fn add(&mut self, other: &Swept) {
        self.incremental_bytes += other.incremental_bytes;
        self.incremental_dirs += other.incremental_dirs;
        self.superseded_bytes += other.superseded_bytes;
        self.superseded_units += other.superseded_units;
    }

    fn is_empty(&self) -> bool {
        self.incremental_dirs == 0 && self.superseded_units == 0
    }

    fn describe(&self) -> String {
        format!(
            "{} of idle incremental state in {} crates, {} in {} superseded builds",
            gigabytes(self.incremental_bytes),
            self.incremental_dirs,
            gigabytes(self.superseded_bytes),
            self.superseded_units
        )
    }
}

/// `cargo seed prune [REPO] [--root DIR] [--idle-minutes N] [--apply]`
///
/// Seeds and targets with a build in flight are left alone.
pub(crate) fn prune(scope: Scope, idle: Duration, apply: bool) -> io::Result<()> {
    let targets = match scope {
        Scope::Repo(repo) => Checkout::new(&repo)
            .worktrees()?
            .into_iter()
            .map(|worktree| worktree.path.join("target"))
            .filter(|target| target.is_dir())
            .collect(),
        Scope::Root(root) => cargo_targets(&root)?,
    };
    let now = SystemTime::now();
    let mut total = Swept::default();
    for target in targets {
        if is_seed(&target) {
            continue;
        }
        if newest_build(&target)?.is_some_and(|built| age(now, built) < BUSY) {
            println!("{}: a build is in flight; kept", target.display());
            continue;
        }
        let swept = sweep(&target, idle, apply)?;
        if !swept.is_empty() {
            println!("{}: {}", target.display(), swept.describe());
        }
        total.add(&swept);
    }
    let verb = if apply {
        "removed"
    } else {
        "would remove (--apply removes)"
    };
    println!("{verb} {}", total.describe());
    Ok(())
}

/// Whether `target` belongs to a seed worktree, which clones read.
fn is_seed(target: &Path) -> bool {
    let Some(worktree) = target.parent() else {
        return false;
    };
    let worktree = real_path(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    Checkout::new(&worktree).worktrees().is_ok_and(|worktrees| {
        worktrees.iter().any(|listed| {
            real_path(&listed.path).is_ok_and(|path| path == worktree)
                && listed.lock_reason.as_deref() == Some(LOCK_REASON)
        })
    })
}

/// Removes from `target` the incremental state of crates no build touched
/// for `idle`, and every build a newer build of the same unit replaced.
///
/// Incremental state is most of a busy target, one directory per crate,
/// feature set, profile and test target; rustc renews a crate's directory
/// with every session, so its mtime is the crate's last build. Superseded
/// builds pile up in `deps/`: a dependency update or a new compiler builds
/// every dependent unit again under a new hash, and cargo keeps the old ones.
/// Either kind costs at most a rebuild if it is needed again.
pub(crate) fn sweep(target: &Path, idle: Duration, apply: bool) -> io::Result<Swept> {
    let now = SystemTime::now();
    let mut swept = Swept::default();
    for crate_dir in incremental_crate_dirs(target)? {
        if age(now, fs::metadata(&crate_dir)?.modified()?) < idle {
            continue;
        }
        swept.incremental_bytes += size(&crate_dir)?;
        swept.incremental_dirs += 1;
        if apply {
            fs::remove_dir_all(&crate_dir)?;
        }
    }
    for profile in profile_dirs(target)? {
        for unit in superseded_units(&profile)? {
            swept.superseded_bytes += remove_unit(&profile, &unit, apply)?;
            swept.superseded_units += 1;
        }
    }
    Ok(swept)
}

/// Fingerprint directory names of the builds in `profile` that a newer build
/// of the same unit replaced: same package, target, features, profile, path,
/// kind and flags, with other dependencies or another compiler.
fn superseded_units(profile: &Path) -> io::Result<Vec<String>> {
    let mut groups: HashMap<String, Vec<(SystemTime, String)>> = HashMap::new();
    for unit in fs::read_dir(profile.join(".fingerprint"))? {
        let unit = unit?;
        let name = unit.file_name().to_string_lossy().into_owned();
        let built = unit.metadata()?.modified()?;
        for record in fs::read_dir(unit.path())? {
            let record = record?.path();
            if record.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Ok(serde_json::Value::Object(mut fields)) =
                serde_json::from_slice(&fs::read(&record)?)
            else {
                continue;
            };
            for varying in ["rustc", "deps", "local"] {
                fields.remove(varying);
            }
            let package = name
                .rsplit_once('-')
                .map_or(name.as_str(), |(package, _)| package);
            let record_name = record.file_name().unwrap_or_default().to_string_lossy();
            let key = format!(
                "{package}\0{record_name}\0{}",
                serde_json::Value::Object(fields)
            );
            groups.entry(key).or_default().push((built, name.clone()));
        }
    }
    let mut superseded = Vec::new();
    for mut builds in groups.into_values() {
        builds.sort();
        let Some(&(newest, _)) = builds.last() else {
            continue;
        };
        superseded.extend(
            builds
                .into_iter()
                .filter(|(built, _)| age(newest, *built) >= SAME_SESSION)
                .map(|(_, name)| name),
        );
    }
    superseded.sort();
    superseded.dedup();
    Ok(superseded)
}

/// Removes one build: its fingerprint, its `deps/` files and its `build/`
/// directory. Returns the bytes they held.
fn remove_unit(profile: &Path, unit: &str, apply: bool) -> io::Result<u64> {
    let Some((_, hash)) = unit.rsplit_once('-') else {
        return Ok(0);
    };
    let mut paths = vec![
        profile.join(".fingerprint").join(unit),
        profile.join("build").join(unit),
    ];
    for entry in fs::read_dir(profile.join("deps"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stem = name.split_once('.').map_or(name.as_str(), |(stem, _)| stem);
        if stem
            .rsplit_once('-')
            .is_some_and(|(_, candidate)| candidate == hash)
        {
            paths.push(entry.path());
        }
    }
    let mut bytes = 0;
    for path in paths.into_iter().filter(|path| path.exists()) {
        bytes += size(&path)?;
        if apply {
            if path.is_dir() {
                fs::remove_dir_all(&path)?
            } else {
                fs::remove_file(&path)?
            }
        }
    }
    Ok(bytes)
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

/// Bytes the files at or under `path` hold.
fn size(path: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        total += size(&entry?.path())?;
    }
    Ok(total)
}
