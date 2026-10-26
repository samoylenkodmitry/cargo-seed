//! Identical build files across cargo targets share disk blocks.
//!
//! Repositories and worktrees that build the same crate with the same flags
//! hold byte-identical files under the same name (`deps/lib<crate>-<hash>`).
//! Each copy after the first becomes a clone of it: APFS clones it, btrfs and
//! XFS share the extents through the kernel's dedupe call, which compares the
//! bytes itself. The content and every file's metadata stay as they were, and
//! a later write to either copy gets blocks of its own.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::target_dir::{cargo_targets, profile_dirs};

/// Smaller files cost more to compare than they free.
const MIN_SIZE: u64 = 64 * 1024;
/// A target written within this long has a build in flight.
const BUSY: Duration = Duration::from_secs(15 * 60);

/// `cargo seed dedupe --root DIR [--apply]`
pub(crate) fn dedupe(root: &Path, apply: bool) -> io::Result<()> {
    let mut groups: HashMap<(String, u64), Vec<PathBuf>> = HashMap::new();
    let now = SystemTime::now();
    for target in cargo_targets(root)? {
        let profiles = profile_dirs(&target)?;
        let busy = profiles.iter().any(|profile| {
            fs::metadata(profile.join(".fingerprint"))
                .and_then(|meta| meta.modified())
                .is_ok_and(|built| now.duration_since(built).unwrap_or_default() < BUSY)
        });
        if busy {
            continue;
        }
        for profile in profiles {
            for entry in fs::read_dir(profile.join("deps"))
                .into_iter()
                .flatten()
                .flatten()
            {
                let meta = entry.metadata()?;
                if meta.is_file() && meta.len() >= MIN_SIZE {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    groups
                        .entry((name, meta.len()))
                        .or_default()
                        .push(entry.path());
                }
            }
        }
    }
    let (mut shared_bytes, mut shared_files) = (0u64, 0usize);
    for ((_, size), paths) in groups {
        let Some((first, rest)) = paths.split_first() else {
            continue;
        };
        let first_block = physical_start(first);
        for copy in rest {
            if first_block.is_some() && physical_start(copy) == first_block {
                continue;
            }
            if !same_bytes(first, copy)? {
                continue;
            }
            if apply && !share(first, copy)? {
                continue;
            }
            shared_bytes += size;
            shared_files += 1;
        }
    }
    let verb = if apply {
        "shared"
    } else {
        "would share (--apply shares)"
    };
    println!(
        "{verb} {:.1} GB in {shared_files} files",
        shared_bytes as f64 / 1e9
    );
    Ok(())
}

fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    let (mut left, mut right) = (vec![0; 1 << 20], vec![0; 1 << 20]);
    loop {
        let read = a.read(&mut left)?;
        if read == 0 {
            return Ok(b.read(&mut right)? == 0);
        }
        b.read_exact(&mut right[..read])?;
        if left[..read] != right[..read] {
            return Ok(false);
        }
    }
}

/// Where a file's first block lies on disk, where the platform says.
#[cfg(target_vendor = "apple")]
fn physical_start(path: &Path) -> Option<u64> {
    use std::os::fd::AsRawFd;
    let file = File::open(path).ok()?;
    let mut map = libc::log2phys {
        l2p_flags: 0,
        l2p_contigbytes: 1,
        l2p_devoffset: 0,
    };
    // SAFETY: the descriptor is open and `map` outlives the call.
    let status = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_LOG2PHYS_EXT, &mut map) };
    (status == 0).then_some(map.l2p_devoffset as u64)
}

#[cfg(not(target_vendor = "apple"))]
fn physical_start(_path: &Path) -> Option<u64> {
    None
}

/// Makes `copy` share `original`'s blocks; false where the filesystem cannot.
#[cfg(target_vendor = "apple")]
fn share(original: &Path, copy: &Path) -> io::Result<bool> {
    use crate::tree_copy::{clone_file_apple, set_times};
    let meta = fs::metadata(copy)?;
    let staged = copy.with_extension("cargo-seed-dedupe");
    if clone_file_apple(original, &staged).is_err() {
        return Ok(false);
    }
    fs::set_permissions(&staged, meta.permissions())?;
    set_times(&staged, meta.accessed()?, meta.modified()?)?;
    fs::rename(&staged, copy)?;
    Ok(true)
}

#[cfg(target_os = "linux")]
fn share(original: &Path, copy: &Path) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct RangeInfo {
        dest_fd: i64,
        dest_offset: u64,
        bytes_deduped: u64,
        status: i32,
        reserved: u32,
    }
    #[repr(C)]
    struct Range {
        src_offset: u64,
        src_length: u64,
        dest_count: u16,
        reserved1: u16,
        reserved2: u32,
        info: [RangeInfo; 1],
    }
    const FIDEDUPERANGE: libc::c_ulong = 0xC018_9436;
    let source = File::open(original)?;
    let target = File::options().write(true).open(copy)?;
    let length = source.metadata()?.len();
    let mut offset = 0;
    while offset < length {
        let chunk = (length - offset).min(16 << 20);
        let mut range = Range {
            src_offset: offset,
            src_length: chunk,
            dest_count: 1,
            reserved1: 0,
            reserved2: 0,
            info: [RangeInfo {
                dest_fd: target.as_raw_fd() as i64,
                dest_offset: offset,
                bytes_deduped: 0,
                status: 0,
                reserved: 0,
            }],
        };
        // SAFETY: both descriptors are open and `range` outlives the call.
        let status = unsafe { libc::ioctl(source.as_raw_fd(), FIDEDUPERANGE, &mut range) };
        if status != 0 || range.info[0].status != 0 || range.info[0].bytes_deduped == 0 {
            return Ok(false);
        }
        offset += range.info[0].bytes_deduped;
    }
    Ok(true)
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn share(_original: &Path, _copy: &Path) -> io::Result<bool> {
    Ok(false)
}
