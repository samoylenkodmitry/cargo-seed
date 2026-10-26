use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// What a committed path is: its object kind and id, as `git ls-tree` prints them.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) kind: Vec<u8>,
    pub(crate) id: Vec<u8>,
}

/// One checkout of a repository, driven through the `git` command line.
pub(crate) struct Checkout {
    root: PathBuf,
}

/// A linked worktree as `git worktree list --porcelain` reports it.
pub(crate) struct Worktree {
    pub(crate) path: PathBuf,
    pub(crate) lock_reason: Option<String>,
}

impl Checkout {
    pub(crate) fn new(root: &Path) -> Checkout {
        Checkout {
            root: root.to_path_buf(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.root);
        // Git exports these to hooks; left set, `git -C <seed>` would read the
        // tree the hook runs for.
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            command.env_remove(name);
        }
        command
    }

    fn output<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(&self, args: I) -> io::Result<Output> {
        self.command().args(args).output()
    }

    /// Stdout of a git command that must succeed.
    pub(crate) fn bytes<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(
        &self,
        args: I,
    ) -> io::Result<Vec<u8>> {
        let output = self.output(args)?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "git in {}: {}",
                self.root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }

    pub(crate) fn text<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(
        &self,
        args: I,
    ) -> io::Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes(args)?)
            .trim()
            .to_owned())
    }

    /// Stdout of a git command that may fail; `None` when it does.
    pub(crate) fn try_text<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(
        &self,
        args: I,
    ) -> Option<String> {
        let output = self.output(args).ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (output.status.success() && !text.is_empty()).then_some(text)
    }

    pub(crate) fn run<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(
        &self,
        args: I,
    ) -> io::Result<()> {
        self.bytes(args).map(drop)
    }

    pub(crate) fn top(&self) -> io::Result<PathBuf> {
        Ok(PathBuf::from(self.text(["rev-parse", "--show-toplevel"])?))
    }

    /// The repository's shared git directory, which every worktree of it sees.
    pub(crate) fn common_dir(&self) -> io::Result<PathBuf> {
        Ok(PathBuf::from(self.text([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])?))
    }

    pub(crate) fn head(&self) -> io::Result<String> {
        self.text(["rev-parse", "HEAD"])
    }

    pub(crate) fn config(&self, key: &str) -> Option<String> {
        self.try_text(["config", "--get", key])
    }

    pub(crate) fn config_all(&self, key: &str) -> Vec<String> {
        self.try_text(["config", "--get-all", key])
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Every file and directory of HEAD by path, symlinks and submodules left out.
    pub(crate) fn committed_entries(&self) -> io::Result<HashMap<Vec<u8>, Entry>> {
        let listing = self.bytes(["ls-tree", "-r", "-t", "-z", "HEAD"])?;
        let mut entries = HashMap::new();
        for record in listing
            .split(|&byte| byte == 0)
            .filter(|record| !record.is_empty())
        {
            let Some(tab) = record.iter().position(|&byte| byte == b'\t') else {
                continue;
            };
            let mut meta = record[..tab].split(|&byte| byte == b' ');
            let (Some(mode), Some(kind), Some(id)) = (meta.next(), meta.next(), meta.next()) else {
                continue;
            };
            if mode == b"120000" || kind == b"commit" {
                continue;
            }
            entries.insert(
                record[tab + 1..].to_vec(),
                Entry {
                    kind: kind.to_vec(),
                    id: id.to_vec(),
                },
            );
        }
        Ok(entries)
    }

    /// Tracked paths whose working copy differs from HEAD, and every directory above them.
    pub(crate) fn edited_paths(&self) -> io::Result<HashSet<Vec<u8>>> {
        let listing = self.bytes(["diff", "--name-only", "-z", "HEAD", "--"])?;
        let mut edited = HashSet::new();
        for path in listing
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
        {
            let mut path = path;
            loop {
                edited.insert(path.to_vec());
                match path.iter().rposition(|&byte| byte == b'/') {
                    Some(slash) => path = &path[..slash],
                    None => break,
                }
            }
        }
        Ok(edited)
    }

    /// Whether the checkout holds anything but build output: changes to
    /// tracked files, or untracked files other than `target/`.
    pub(crate) fn has_work(&self) -> io::Result<bool> {
        let status = self.text(["status", "--porcelain=v1", "--untracked-files=normal"])?;
        Ok(status.lines().any(|line| line != "?? target/"))
    }

    pub(crate) fn worktrees(&self) -> io::Result<Vec<Worktree>> {
        let listing = self.text(["worktree", "list", "--porcelain"])?;
        let mut worktrees: Vec<Worktree> = Vec::new();
        for line in listing.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                worktrees.push(Worktree {
                    path: PathBuf::from(path),
                    lock_reason: None,
                });
            } else if let Some(reason) = line.strip_prefix("locked")
                && let Some(worktree) = worktrees.last_mut()
            {
                worktree.lock_reason = Some(reason.trim_start().to_owned());
            }
        }
        Ok(worktrees)
    }
}
