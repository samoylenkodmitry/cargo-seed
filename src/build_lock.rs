use std::fs::File;
use std::io;
use std::path::Path;

use crate::target_dir::profile_dirs;

/// Cargo's build locks on one target, held shared.
///
/// A build holds `.cargo-build-lock` (and `.cargo-artifact-lock`) exclusive,
/// and older cargo holds `.cargo-lock` exclusive, so holding all three shared
/// means no build runs in the target, and none starts until this drops.
pub(crate) struct BuildLock {
    _files: Vec<File>,
}

impl BuildLock {
    /// The locks of every profile in `target`, or `None` while a build runs.
    pub(crate) fn try_shared(target: &Path) -> io::Result<Option<BuildLock>> {
        let mut files = Vec::new();
        for profile in profile_dirs(target)? {
            for name in [".cargo-build-lock", ".cargo-artifact-lock", ".cargo-lock"] {
                let Ok(file) = File::open(profile.join(name)) else {
                    continue;
                };
                if file.try_lock_shared().is_err() {
                    return Ok(None);
                }
                files.push(file);
            }
        }
        Ok(Some(BuildLock { _files: files }))
    }
}
