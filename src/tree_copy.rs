use std::fs;
#[cfg(windows)]
use std::fs::{File, FileTimes};
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// Copies the directory tree `src` to `dst`, which must not exist, keeping
/// every file's and directory's times: cargo compares them.
///
/// APFS clones the whole tree in one call. Elsewhere each file goes through
/// `fs::copy`, which shares blocks where the filesystem can (a reflink on
/// btrfs and XFS, block cloning on ReFS) and copies bytes where it cannot.
pub(crate) fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    if clone_file_apple(src, dst).is_ok() {
        return Ok(());
    }
    copy_entries(src, dst)
}

/// One `clonefile(2)` call: a file, or a whole directory tree, that shares
/// `src`'s blocks.
#[cfg(target_vendor = "apple")]
pub(crate) fn clone_file_apple(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32)
        -> libc::c_int;
    }
    const CLONE_NOFOLLOW: u32 = 1;
    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both arguments are NUL-terminated paths that outlive the call.
    match unsafe { clonefile(src.as_ptr(), dst.as_ptr(), CLONE_NOFOLLOW) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn copy_entries(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_entries(&from, &to)?;
        } else if kind.is_symlink() {
            copy_symlink(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
            copy_times(&from, &to)?;
        }
    }
    // Last: creating the entries above moved the directory's own times.
    copy_times(src, dst)
}

#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(from)?, to)
}

#[cfg(windows)]
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
    let target = fs::read_link(from)?;
    if fs::metadata(from).is_ok_and(|meta| meta.is_dir()) {
        std::os::windows::fs::symlink_dir(target, to)
    } else {
        std::os::windows::fs::symlink_file(target, to)
    }
}

/// Gives `to` the access and modification times of `from`.
pub(crate) fn copy_times(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::metadata(from)?;
    set_times(to, meta.accessed()?, meta.modified()?)
}

/// Sets a file's or directory's times by path, whatever its permissions.
#[cfg(unix)]
pub(crate) fn set_times(path: &Path, accessed: SystemTime, modified: SystemTime) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    fn timespec(time: SystemTime) -> libc::timespec {
        let since = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        libc::timespec {
            tv_sec: since.as_secs() as libc::time_t,
            tv_nsec: since.subsec_nanos() as _,
        }
    }
    let path = CString::new(path.as_os_str().as_bytes())?;
    let times = [timespec(accessed), timespec(modified)];
    // SAFETY: a NUL-terminated path and two timespecs that outlive the call.
    match unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(windows)]
pub(crate) fn set_times(path: &Path, accessed: SystemTime, modified: SystemTime) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    // Opens directories too, and needs only the right to write attributes.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    let file = File::options()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    file.set_times(
        FileTimes::new()
            .set_accessed(accessed)
            .set_modified(modified),
    )
}
