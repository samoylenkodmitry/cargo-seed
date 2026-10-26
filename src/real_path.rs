use std::io;
use std::path::{Path, PathBuf};

/// The absolute path with links resolved, in the form git and cargo write.
///
/// Windows canonicalizes to a `\\?\C:\...` path, which git rejects and which
/// matches none of the `C:\...` paths in cargo's records.
pub(crate) fn real_path(path: &Path) -> io::Result<PathBuf> {
    let resolved = path.canonicalize()?;
    #[cfg(windows)]
    if let Some(plain) = resolved
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        && plain.as_bytes().get(1) == Some(&b':')
    {
        return Ok(PathBuf::from(plain));
    }
    Ok(resolved)
}
