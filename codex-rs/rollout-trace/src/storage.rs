//! Filesystem boundary shared by trace capture and offline admission.

use std::fs;
use std::fs::File;
use std::path::Path;

use anyhow::Result;
use anyhow::ensure;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Require a real private directory beneath non-replaceable ancestors.
///
/// Unix ownership and mode checks protect against other users, not malicious
/// code running as the same user. Linux also verifies the process owner through
/// procfs. Other platforms retain structural checks; Windows ACL qualification
/// is a separate requirement, not something Unix mode checks can establish.
pub(crate) fn validate_private_dir(path: &Path) -> Result<()> {
    let path = std::path::absolute(path)?;
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_dir(),
        "trace directory must not be linked or special"
    );
    #[cfg(unix)]
    {
        ensure!(
            metadata.mode() & 0o077 == 0,
            "trace directory must be private"
        );
        #[cfg(target_os = "linux")]
        ensure!(
            metadata.uid() == linux_effective_uid()?,
            "trace directory must belong to the current user"
        );
    }
    for ancestor in path.ancestors().skip(1) {
        let ancestor_metadata = fs::symlink_metadata(ancestor)?;
        ensure!(
            ancestor_metadata.is_dir(),
            "trace ancestor must not be linked or special"
        );
        #[cfg(unix)]
        {
            ensure!(
                ancestor_metadata.uid() == 0 || ancestor_metadata.uid() == metadata.uid(),
                "trace ancestor has a different owner"
            );
            // A trusted sticky directory (normally /tmp) protects a user's
            // private child from replacement by another unprivileged user.
            ensure!(
                ancestor_metadata.mode() & 0o022 == 0 || ancestor_metadata.mode() & 0o1000 != 0,
                "trace ancestor permits replacement by other users"
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_effective_uid() -> Result<u32> {
    use std::io::Read;
    // Disabling process dumping can make /proc/self's inode root-owned even
    // though the process still runs as the ordinary user.
    // The kernel status field gives real/effective/saved/filesystem UIDs instead.
    let mut prefix = Vec::new();
    File::open("/proc/self/status")?
        .take(4096)
        .read_to_end(&mut prefix)?;
    parse_effective_uid(&prefix)
}

#[cfg(target_os = "linux")]
fn parse_effective_uid(status: &[u8]) -> Result<u32> {
    let line = status
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"Uid:"))
        .ok_or_else(|| anyhow::anyhow!("process effective owner unavailable"))?;
    let text = std::str::from_utf8(line)
        .map_err(|_| anyhow::anyhow!("process effective owner is invalid"))?;
    let values = text
        .split_ascii_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| anyhow::anyhow!("process effective owner is invalid"))?;
    ensure!(values.len() == 4, "process owner fields are incomplete");
    Ok(values[1])
}

#[cfg(all(test, target_os = "linux"))]
#[path = "storage_tests.rs"]
mod tests;

/// Validate an opened regular file before any producer write or consumer read.
///
/// The caller must validate the directory and reject a symlink at the file path
/// before opening. Consumers additionally compare path/opened-file identity and
/// metadata after reading; the file descriptor alone cannot detect path aliases.
pub(crate) fn validate_private_file(file: &File, directory: &Path) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "trace file must be regular");
    #[cfg(unix)]
    {
        let directory_metadata = fs::symlink_metadata(directory)?;
        ensure!(
            directory_metadata.is_dir(),
            "trace directory must not be linked or special"
        );
        ensure!(metadata.nlink() == 1, "trace file must not have hard links");
        ensure!(metadata.mode() & 0o077 == 0, "trace file must be private");
        ensure!(
            metadata.uid() == directory_metadata.uid(),
            "trace file has a different owner"
        );
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}
