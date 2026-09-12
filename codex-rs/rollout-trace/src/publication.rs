//! Private, complete publication of a diagnostic reduced trace.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use uuid::Uuid;

use crate::bundle::MANIFEST_FILE_NAME;
use crate::bundle::PAYLOADS_DIR_NAME;
use crate::bundle::RAW_EVENT_LOG_FILE_NAME;
use crate::limits::MAX_BUNDLE_BYTES;
use crate::replay_bundle;
use crate::storage::validate_private_dir;
use crate::storage::validate_private_file;
use crate::writer::JsonLayout;
use crate::writer::encode_record;

/// Admit and reduce a local bundle, then atomically publish its complete JSON.
///
/// The output parent must already be a real private directory. Existing regular
/// private state files may be replaced; bundle inputs, links and special files
/// may not. Serialized output is bounded by the bundle byte limit. Errors before
/// rename preserve any existing output; temporary-file cleanup failures are
/// reported. Rename provides atomic visibility, not fsync crash durability.
///
/// Unix ownership, mode and link checks protect against other users. Concurrent
/// mutation by malicious code with the same UID is outside this contract, as is
/// Windows ACL qualification (only structural checks apply on non-Unix hosts).
pub fn reduce_to_file(bundle_dir: &Path, output: &Path) -> Result<()> {
    let output = std::path::absolute(output).context("resolve reduced-state output")?;
    let parent = output
        .parent()
        .context("reduced-state output needs a parent")?;
    validate_private_dir(parent)?;
    // Validate before canonicalizing so an ancestor symlink is never accepted
    // merely because its resolved destination happens to be private.
    let parent = fs::canonicalize(parent).context("resolve reduced-state directory")?;
    let output = parent.join(
        output
            .file_name()
            .context("reduced-state output needs a filename")?,
    );
    validate_output(&output, &parent)?;

    validate_private_dir(bundle_dir)?;
    let bundle_dir = fs::canonicalize(bundle_dir).context("resolve trace bundle directory")?;
    ensure!(
        output != bundle_dir.join(MANIFEST_FILE_NAME)
            && output != bundle_dir.join(RAW_EVENT_LOG_FILE_NAME)
            && !output.starts_with(bundle_dir.join(PAYLOADS_DIR_NAME)),
        "reduced-state output must not replace trace bundle inputs"
    );

    let trace = replay_bundle(&bundle_dir)?;
    let bytes = encode_record(&trace, JsonLayout::Pretty, MAX_BUNDLE_BYTES)?;
    PendingState::create(&parent)?.publish(&output, &bytes)
}

fn validate_output(output: &Path, parent: &Path) -> Result<()> {
    match fs::symlink_metadata(output) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "reduced-state output must be a regular file"
            );
            let file = File::open(output).context("inspect existing reduced-state output")?;
            validate_private_file(&file, parent)?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect reduced-state output path"),
    }
    Ok(())
}

/// Owns an exclusively created temporary file until publication or cleanup.
struct PendingState {
    path: Option<PathBuf>,
    // Taking the handle before removal also permits cleanup on Windows.
    file: Option<File>,
}

impl PendingState {
    fn create(parent: &Path) -> Result<Self> {
        validate_private_dir(parent)?;
        let path = parent.join(format!(".state-{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .context("create private reduced-state temporary file")?;
        Ok(Self {
            path: Some(path),
            file: Some(file),
        })
    }

    fn publish(mut self, output: &Path, bytes: &[u8]) -> Result<()> {
        let result: Result<()> = (|| {
            let parent = output
                .parent()
                .context("reduced-state output needs a parent")?;
            let file = self
                .file
                .as_mut()
                .context("pending reduced-state file is closed")?;
            validate_private_file(file, parent)?;
            file.write_all(bytes)
                .context("write complete reduced state")?;
            file.flush().context("flush reduced state")?;
            self.file.take();
            validate_private_dir(parent)?;
            validate_output(output, parent)?;
            let path = self
                .path
                .as_ref()
                .context("pending reduced-state path is absent")?;
            fs::rename(path, output).context("publish complete reduced state")?;
            self.path.take();
            Ok(())
        })();
        if let Err(error) = result {
            return match self.discard() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(cleanup)),
            };
        }
        Ok(())
    }

    fn discard(&mut self) -> Result<()> {
        self.file.take();
        if let Some(path) = &self.path {
            fs::remove_file(path).context("remove incomplete reduced-state temporary file")?;
            self.path.take();
        }
        Ok(())
    }
}

impl Drop for PendingState {
    fn drop(&mut self) {
        // Normal failures report cleanup errors above. This is a best-effort
        // fallback during unwinding; successful rename/removal disarms it.
        let _ = self.discard();
    }
}

#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;
