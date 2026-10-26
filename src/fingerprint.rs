//! Cargo's records in a cloned target, pointed at the new tree or dropped.
//!
//! Cargo stores a local package's sources relative to the package root and
//! rewrites a build script's own OUT_DIR when the target moves, so most of a
//! cloned target is right for the new tree as it is. These records are not:
//!
//! * A fingerprint's dep-info that lists a file by absolute path in the seed
//!   (`clippy.toml`, a file a proc macro included by absolute path). Cargo
//!   would keep comparing the seed's copy, so the path moves to the new tree.
//! * A build script output that names a path in the seed. Paths into the
//!   cloned target move with it; a watched seed source makes the script rerun.
//! * A unit whose code holds a seed path. rustc's own dep-info lists every
//!   variable a crate read with `env!`, and cargo leaves the ones it sets
//!   itself, CARGO_MANIFEST_DIR among them, out of its fingerprint (cargo
//!   #8693), so the unit rebuilds. OUT_DIR moves with the target, as cargo
//!   assumes too, and a unit that only checks produces no code.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{MAIN_SEPARATOR, Path};

use crate::tree_copy::set_times;

/// What the records change: counts for the report.
#[derive(Default)]
pub(crate) struct Moved {
    pub(crate) records: usize,
    pub(crate) rebuilds: usize,
}

/// The seed's and the tree's roots as the bytes cargo writes into its records.
struct Roots {
    seed: Vec<u8>,
    tree: Vec<u8>,
}

impl Roots {
    fn new(seed: &Path, tree: &Path) -> Roots {
        Roots {
            seed: path_bytes(seed),
            tree: path_bytes(tree),
        }
    }

    /// Whether `data` names the seed anywhere, as a root or a path below it.
    fn named_in(&self, data: &[u8]) -> bool {
        find(data, &self.seed).is_some()
    }

    /// `path` moved to the tree when it is the seed's root or below it.
    fn moved(&self, path: &[u8]) -> Option<Vec<u8>> {
        let rest = path.strip_prefix(self.seed.as_slice())?;
        (rest.is_empty() || rest.first() == Some(&(MAIN_SEPARATOR as u8)))
            .then(|| [self.tree.as_slice(), rest].concat())
    }

    fn below_seed(&self, child: &str) -> Vec<u8> {
        [
            self.seed.as_slice(),
            &[MAIN_SEPARATOR as u8],
            child.as_bytes(),
        ]
        .concat()
    }

    fn below_tree(&self, child: &str) -> Vec<u8> {
        [
            self.tree.as_slice(),
            &[MAIN_SEPARATOR as u8],
            child.as_bytes(),
        ]
        .concat()
    }
}

fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn replace_all(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut rest = data;
    while let Some(at) = find(rest, from) {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(to);
        rest = &rest[at + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Points every profile's records in `target` (the tree's clone) at `tree`.
pub(crate) fn move_fingerprints(target: &Path, seed: &Path, tree: &Path) -> io::Result<Moved> {
    let roots = Roots::new(seed, tree);
    let mut moved = Moved::default();
    for profile in profile_dirs(target)? {
        let holds_seed_path = units_holding_seed_paths(&profile, &roots)?;
        let fingerprints = profile.join(".fingerprint");
        for unit in fs::read_dir(&fingerprints)? {
            let unit = unit?.path();
            move_unit(&profile, &unit, &roots, &holds_seed_path, &mut moved)?;
        }
    }
    Ok(moved)
}

/// Directories that hold a `.fingerprint`: `<target>/<profile>` and `<target>/<triple>/<profile>`.
fn profile_dirs(target: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let mut found = Vec::new();
    for level in fs::read_dir(target)? {
        let level = level?.path();
        if !level.is_dir() {
            continue;
        }
        if level.join(".fingerprint").is_dir() {
            found.push(level.clone());
        }
        for nested in fs::read_dir(&level)? {
            let nested = nested?.path();
            if nested.join(".fingerprint").is_dir() {
                found.push(nested);
            }
        }
    }
    Ok(found)
}

fn unit_hash(name: &str) -> &str {
    name.rsplit_once('-').map_or(name, |(_, hash)| hash)
}

fn move_unit(
    profile: &Path,
    unit: &Path,
    roots: &Roots,
    holds_seed_path: &HashSet<String>,
    moved: &mut Moved,
) -> io::Result<()> {
    let name = unit
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let output = profile.join("build").join(&name).join("output");
    let mut rewrites = Vec::new();
    let mut stale = holds_seed_path.contains(unit_hash(&name));
    if !stale {
        for record in fs::read_dir(unit)? {
            let record = record?.path();
            let data = fs::read(&record)?;
            if !roots.named_in(&data) {
                continue;
            }
            let is_dep_info = record
                .file_name()
                .is_some_and(|file| file.to_string_lossy().starts_with("dep-"));
            match is_dep_info.then(|| move_dep_info(&data, roots)).flatten() {
                Some(data) => rewrites.push((record, data)),
                None => {
                    stale = true;
                    break;
                }
            }
        }
    }
    if !stale && output.is_file() {
        let original = fs::read(&output)?;
        let data = replace_all(
            &original,
            &roots.below_seed("target"),
            &roots.below_tree("target"),
        );
        if roots.named_in(&data) {
            stale = true;
        } else if data != original {
            rewrites.push((output.clone(), data));
        }
    }
    if stale {
        fs::remove_dir_all(unit)?;
        if output.is_file() {
            // Cargo fingerprints a build script that is about to run from its
            // previous output's watch list; seed paths left there would change
            // the run's fingerprint once more on the next build.
            let data = replace_all(&fs::read(&output)?, &roots.seed, &roots.tree);
            fs::write(&output, data)?;
        }
        moved.rebuilds += 1;
        return Ok(());
    }
    for (record, data) in rewrites {
        // Cargo reads a dep-info file's and a build script output's own mtime
        // as the time of the build they describe, so the rewrite keeps it.
        let meta = fs::metadata(&record)?;
        fs::write(&record, data)?;
        set_times(&record, meta.accessed()?, meta.modified()?)?;
        moved.records += 1;
    }
    Ok(())
}

// Cargo's fingerprint dep-info, format version 1 (cargo's fingerprint/dep_info.rs):
// a marker (u32 1, u8 0xff), u8 version, u32 file count, then per file u8 path
// type, u32-length path, u8 checksum flag [u64 size, u32-length checksum], then
// u32 env count and per entry u32-length key, u8 flag [u32-length value].
const DEP_INFO_MARKER: &[u8] = b"\x01\x00\x00\x00\xff\x01";

/// The dep-info record with its files moved to the tree; `None` when it is
/// not format version 1, or when its environment part names the seed, whose
/// value differs in the tree.
fn move_dep_info(data: &[u8], roots: &Roots) -> Option<Vec<u8>> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, tail) = (self.0.get(..n)?, self.0.get(n..)?);
            self.0 = tail;
            Some(head)
        }
        fn length(&mut self) -> Option<usize> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
        }
        fn bytes(&mut self) -> Option<&'a [u8]> {
            let n = self.length()?;
            self.take(n)
        }
    }
    fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
    }

    let mut reader = Reader(data.strip_prefix(DEP_INFO_MARKER)?);
    let mut out = DEP_INFO_MARKER.to_vec();
    let files = reader.length()?;
    out.extend_from_slice(&(files as u32).to_le_bytes());
    for _ in 0..files {
        out.extend_from_slice(reader.take(1)?);
        let path = reader.bytes()?;
        put_bytes(
            &mut out,
            &roots.moved(path).unwrap_or_else(|| path.to_vec()),
        );
        let has_checksum = reader.take(1)?;
        out.extend_from_slice(has_checksum);
        if has_checksum != [0] {
            out.extend_from_slice(reader.take(8)?);
            put_bytes(&mut out, reader.bytes()?);
        }
    }
    if roots.named_in(reader.0) {
        return None;
    }
    out.extend_from_slice(reader.0);
    Some(out)
}

/// Hashes of the units in `profile` whose code may hold a path into the seed.
fn units_holding_seed_paths(profile: &Path, roots: &Roots) -> io::Result<HashSet<String>> {
    let seed_target = roots.below_seed("target");
    let mut hashes = HashSet::new();
    for dep_info in rustc_dep_infos(profile)? {
        let data = fs::read(&dep_info)?;
        let stem = dep_info
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let hash = unit_hash(&stem).to_owned();
        if checks_only(&data, &hash) {
            continue;
        }
        let names_seed = data
            .split(|&byte| byte == b'\n')
            .filter_map(|line| line.strip_prefix(b"# env-dep:"))
            .any(|assignment| {
                let (name, value) = match find(assignment, b"=") {
                    Some(at) => (&assignment[..at], &assignment[at + 1..]),
                    None => (assignment, &[][..]),
                };
                roots.named_in(value) && !(name == b"OUT_DIR" && value.starts_with(&seed_target))
            });
        if names_seed {
            hashes.insert(hash);
        }
    }
    Ok(hashes)
}

/// rustc's dep-info files: `deps/*.d` and the build scripts' `build/*/*.d`.
fn rustc_dep_infos(profile: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let mut found = Vec::new();
    let mut collect = |dir: &Path| -> io::Result<()> {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries {
                let path = entry?.path();
                if path.extension().is_some_and(|ext| ext == "d") {
                    found.push(path);
                }
            }
        }
        Ok(())
    };
    collect(&profile.join("deps"))?;
    if let Ok(builds) = fs::read_dir(profile.join("build")) {
        for build in builds {
            collect(&build?.path())?;
        }
    }
    Ok(found)
}

/// Whether every output the dep-info names is metadata: a check, no code.
/// Each output is a rule of its own, and only outputs carry the unit's hash.
fn checks_only(data: &[u8], hash: &str) -> bool {
    let marker = format!("-{hash}");
    // A rule ends at ": " or at a closing ':'; a drive letter's colon is
    // followed by a separator.
    let rule_of = |line: &'_ [u8]| -> Option<Vec<u8>> {
        match find(line, b": ") {
            Some(at) => Some(line[..at].to_vec()),
            None => line.strip_suffix(b":").map(<[u8]>::to_vec),
        }
    };
    data.split(|&byte| byte == b'\n')
        .filter(|line| !line.starts_with(b"#"))
        .filter_map(rule_of)
        .filter(|rule| {
            let base = rule
                .rsplit(|&byte| byte == b'/' || byte == b'\\')
                .next()
                .unwrap_or(rule);
            find(base, marker.as_bytes()).is_some()
        })
        .all(|output| output.ends_with(b".rmeta") || output.ends_with(b".d"))
}
