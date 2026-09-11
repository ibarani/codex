use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::PendingState;
use super::reduce_to_file;
use crate::model::RolloutStatus;
use crate::payload::RawPayloadKind;
use crate::raw_event::RawTraceEventPayload;
use crate::reducer::test_support::ROOT_THREAD_ID;
use crate::reducer::test_support::create_started_writer;
use crate::reducer::test_support::private_tempdir;
use crate::replay_bundle;

fn assert_no_temporary_files(directory: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(directory)? {
        assert!(!entry?.file_name().to_string_lossy().starts_with(".state-"));
    }
    Ok(())
}

#[test]
fn publishes_complete_state_and_regenerates_after_more_events() -> anyhow::Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    let output = temp.path().join("state.json");
    reduce_to_file(temp.path(), &output)?;
    let first = fs::read(&output)?;
    assert_eq!(
        serde_json::from_slice::<Value>(&first)?,
        serde_json::to_value(replay_bundle(temp.path())?)?
    );

    writer.append(RawTraceEventPayload::ThreadEnded {
        thread_id: ROOT_THREAD_ID.into(),
        status: RolloutStatus::Completed,
    })?;
    writer.append(RawTraceEventPayload::RolloutEnded {
        status: RolloutStatus::Completed,
    })?;
    reduce_to_file(temp.path(), &output)?;
    let second = fs::read(&output)?;
    assert_ne!(first, second);
    assert_eq!(
        serde_json::from_slice::<Value>(&second)?,
        serde_json::to_value(replay_bundle(temp.path())?)?
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(&output)?;
        assert_eq!((metadata.mode() & 0o777, metadata.nlink()), (0o600, 1));
    }
    assert_no_temporary_files(temp.path())
}

#[test]
fn custom_output_requires_an_existing_private_directory() -> anyhow::Result<()> {
    let bundle = private_tempdir()?;
    let _writer = create_started_writer(&bundle)?;
    let destination = private_tempdir()?;
    let output = destination.path().join("custom.json");
    reduce_to_file(bundle.path(), &output)?;
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&output)?)?,
        serde_json::to_value(replay_bundle(bundle.path())?)?
    );
    let missing = destination.path().join("missing");
    assert!(reduce_to_file(bundle.path(), &missing.join("state.json")).is_err());
    assert!(!missing.exists());
    assert_no_temporary_files(destination.path())
}

#[test]
fn bundle_inputs_and_parent_path_aliases_are_never_replaced() -> anyhow::Result<()> {
    let bundle = private_tempdir()?;
    let writer = create_started_writer(&bundle)?;
    let payload = writer.write_json_payload(RawPayloadKind::ToolResult, &json!("evidence"))?;
    for relative in [
        "manifest.json",
        "trace.jsonl",
        payload.path.as_str(),
        "payloads/../manifest.json",
        "payloads/../trace.jsonl",
    ] {
        let output = bundle.path().join(relative);
        let before = fs::read(&output)?;
        assert!(reduce_to_file(bundle.path(), &output).is_err());
        assert_eq!(fs::read(&output)?, before);
    }
    let future_payload = bundle.path().join("payloads/999.json");
    assert!(reduce_to_file(bundle.path(), &future_payload).is_err());
    assert!(!future_payload.exists());
    assert_no_temporary_files(bundle.path())?;
    assert_no_temporary_files(&bundle.path().join("payloads"))
}

#[test]
fn invalid_source_preserves_the_previous_complete_output() -> anyhow::Result<()> {
    let bundle = private_tempdir()?;
    let writer = create_started_writer(&bundle)?;
    let output = bundle.path().join("state.json");
    reduce_to_file(bundle.path(), &output)?;
    let before = fs::read(&output)?;
    drop(writer);
    fs::write(
        bundle.path().join("trace.jsonl"),
        b"invalid synthetic JSON\n",
    )?;
    assert!(reduce_to_file(bundle.path(), &output).is_err());
    assert_eq!(fs::read(&output)?, before);
    assert_no_temporary_files(bundle.path())
}

#[test]
fn failed_write_removes_partial_temporary_state_and_preserves_output() -> anyhow::Result<()> {
    let bundle = private_tempdir()?;
    let _writer = create_started_writer(&bundle)?;
    let output = bundle.path().join("state.json");
    reduce_to_file(bundle.path(), &output)?;
    let before = fs::read(&output)?;
    let mut pending = PendingState::create(bundle.path())?;
    pending
        .file
        .as_mut()
        .expect("temporary file is open")
        .write_all(b"partial JSON")?;
    // A read-only descriptor makes the next write fail without relying on disk
    // exhaustion or the current user's ability to bypass directory permissions.
    pending.file = Some(File::open(
        pending.path.as_ref().expect("temporary path is owned"),
    )?);
    assert!(pending.publish(&output, b"replacement JSON").is_err());
    assert_eq!(fs::read(&output)?, before);
    assert_no_temporary_files(bundle.path())
}

#[test]
fn publication_failure_cleans_temporary_state_without_replacing_a_directory() -> anyhow::Result<()>
{
    let destination = private_tempdir()?;
    let pending = PendingState::create(destination.path())?;
    let output = destination.path().join("state.json");
    fs::create_dir(&output)?;
    assert!(pending.publish(&output, b"complete JSON").is_err());
    assert!(output.is_dir());
    assert_no_temporary_files(destination.path())
}

#[cfg(unix)]
#[test]
fn cleanup_failure_preserves_primary_failure_and_previous_output() -> anyhow::Result<()> {
    let bundle = private_tempdir()?;
    let _writer = create_started_writer(&bundle)?;
    let output = bundle.path().join("state.json");
    reduce_to_file(bundle.path(), &output)?;
    let before = fs::read(&output)?;
    let mut pending = PendingState::create(bundle.path())?;
    let pending_path = pending.path.clone().expect("temporary path is owned");
    pending.file = Some(File::open(&pending_path)?);
    // Keep a valid read-only descriptor for the primary write failure. Replacing
    // its old pathname with a directory makes cleanup fail even when run as root.
    fs::rename(&pending_path, bundle.path().join("detached-private.json"))?;
    fs::create_dir(&pending_path)?;

    let error = pending
        .publish(&output, b"replacement JSON")
        .expect_err("both write and cleanup must fail");
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("write complete reduced state"));
    assert!(diagnostic.contains("remove incomplete reduced-state temporary file"));
    assert_eq!(fs::read(&output)?, before);
    assert!(pending_path.is_dir());
    fs::remove_dir(&pending_path)?;
    assert_no_temporary_files(bundle.path())
}

#[cfg(unix)]
#[test]
fn symlink_hardlink_and_shared_targets_preserve_original_bytes() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;

    let bundle = private_tempdir()?;
    let _writer = create_started_writer(&bundle)?;
    let destination = private_tempdir()?;
    let victim = destination.path().join("prior.json");
    reduce_to_file(bundle.path(), &victim)?;
    let before = fs::read(&victim)?;

    let linked = destination.path().join("linked.json");
    symlink(&victim, &linked)?;
    assert!(reduce_to_file(bundle.path(), &linked).is_err());
    assert!(fs::symlink_metadata(&linked)?.file_type().is_symlink());
    assert_eq!(fs::read(&victim)?, before);

    let hardlinked = destination.path().join("hardlinked.json");
    fs::hard_link(&victim, &hardlinked)?;
    assert!(reduce_to_file(bundle.path(), &hardlinked).is_err());
    assert_eq!(fs::read(&victim)?, before);
    fs::remove_file(&hardlinked)?;

    fs::set_permissions(&victim, fs::Permissions::from_mode(0o644))?;
    assert!(reduce_to_file(bundle.path(), &victim).is_err());
    assert_eq!(fs::read(&victim)?, before);

    let absent = destination.path().join("absent.json");
    let dangling = destination.path().join("dangling.json");
    symlink(&absent, &dangling)?;
    assert!(reduce_to_file(bundle.path(), &dangling).is_err());
    assert!(!absent.exists());
    assert_no_temporary_files(destination.path())
}

#[cfg(unix)]
#[test]
fn linked_or_shared_output_directories_are_rejected() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;

    let bundle = private_tempdir()?;
    let _writer = create_started_writer(&bundle)?;
    let destination = private_tempdir()?;
    let linked = bundle.path().join("linked-directory");
    symlink(destination.path(), &linked)?;
    assert!(reduce_to_file(bundle.path(), &linked.join("state.json")).is_err());
    assert!(!destination.path().join("state.json").exists());

    fs::set_permissions(destination.path(), fs::Permissions::from_mode(0o755))?;
    assert!(reduce_to_file(bundle.path(), &destination.path().join("state.json")).is_err());
    assert!(!destination.path().join("state.json").exists());
    assert_no_temporary_files(destination.path())
}

#[cfg(unix)]
#[test]
fn bundle_input_hardlink_aliases_are_rejected_before_any_mutation() -> anyhow::Result<()> {
    let bundle = private_tempdir()?;
    let _writer = create_started_writer(&bundle)?;
    let destination = private_tempdir()?;
    let manifest = bundle.path().join("manifest.json");
    let before = fs::read(&manifest)?;
    let alias = destination.path().join("alias.json");
    fs::hard_link(&manifest, &alias)?;
    assert!(reduce_to_file(bundle.path(), &alias).is_err());
    assert_eq!(fs::read(&manifest)?, before);
    assert_eq!(fs::read(&alias)?, before);
    assert_no_temporary_files(destination.path())
}

#[cfg(unix)]
#[test]
fn special_output_files_are_rejected_before_opening() -> anyhow::Result<()> {
    use std::os::unix::net::UnixListener;

    let bundle = private_tempdir()?;
    let _writer = create_started_writer(&bundle)?;
    let destination = private_tempdir()?;
    let output = destination.path().join("state.json");
    let _listener = UnixListener::bind(&output)?;
    assert!(reduce_to_file(bundle.path(), &output).is_err());
    assert!(!fs::symlink_metadata(&output)?.is_file());
    assert_no_temporary_files(destination.path())
}
