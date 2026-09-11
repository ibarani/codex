//! Bounded admission of a trace's complete referenced evidence before reduction.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::fs::Metadata;
use std::io::Read;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use serde::de::IgnoredAny;

use crate::bundle::MANIFEST_FILE_NAME;
use crate::bundle::PAYLOADS_DIR_NAME;
use crate::bundle::RAW_EVENT_LOG_FILE_NAME;
use crate::bundle::TRACE_MANIFEST_SCHEMA_VERSION;
use crate::bundle::TraceBundleManifest;
use crate::limits::MAX_BUNDLE_BYTES;
use crate::limits::MAX_EVENT_COUNT;
use crate::limits::MAX_PAYLOAD_COUNT;
use crate::limits::MAX_RECORD_BYTES;
use crate::payload::RawPayloadRef;
use crate::raw_event::RAW_TRACE_EVENT_SCHEMA_VERSION;
use crate::raw_event::RawTraceEvent;
use crate::raw_event::RawTraceEventPayload;
use crate::storage::validate_private_dir;
use crate::storage::validate_private_file;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub(super) struct BundleSnapshot {
    pub(super) manifest: TraceBundleManifest,
    pub(super) manifest_json: Vec<u8>,
    pub(super) event_log_jsonl: Vec<u8>,
    pub(super) events: Vec<RawTraceEvent>,
    pub(super) payloads: BTreeMap<String, Vec<u8>>,
}

pub(super) fn read_bundle(bundle_dir: &Path) -> Result<BundleSnapshot> {
    validate_private_dir(bundle_dir)?;
    let payloads_dir = bundle_dir.join(PAYLOADS_DIR_NAME);
    validate_private_dir(&payloads_dir)?;
    let mut remaining = MAX_BUNDLE_BYTES;
    let manifest_path = bundle_dir.join(MANIFEST_FILE_NAME);
    let event_log_path = bundle_dir.join(RAW_EVENT_LOG_FILE_NAME);
    let manifest_stamp = fs::symlink_metadata(&manifest_path)?;
    let manifest_json = read_file(&manifest_path, bundle_dir, MAX_RECORD_BYTES, &mut remaining)
        .context("read trace manifest")?;
    let manifest_text =
        std::str::from_utf8(&manifest_json).map_err(|_| anyhow!("trace manifest must be UTF-8"))?;
    let manifest: TraceBundleManifest =
        serde_json::from_str(manifest_text).map_err(|_| anyhow!("invalid trace manifest JSON"))?;
    ensure!(
        manifest.schema_version == TRACE_MANIFEST_SCHEMA_VERSION,
        "unsupported trace manifest schema"
    );
    ensure!(
        manifest.raw_event_log == RAW_EVENT_LOG_FILE_NAME
            && manifest.payloads_dir == PAYLOADS_DIR_NAME,
        "unsupported trace bundle layout"
    );
    ensure!(
        !manifest.trace_id.is_empty()
            && !manifest.rollout_id.is_empty()
            && !manifest.root_thread_id.is_empty(),
        "trace manifest identity must not be empty"
    );

    let event_log_stamp = fs::symlink_metadata(&event_log_path)?;
    let event_log_jsonl = read_file(
        &event_log_path,
        bundle_dir,
        MAX_BUNDLE_BYTES,
        &mut remaining,
    )
    .context("read trace event log")?;
    ensure!(
        event_log_jsonl.is_empty() || event_log_jsonl.ends_with(b"\n"),
        "trace event log has an incomplete final record"
    );
    let mut events = Vec::new();
    let mut references = BTreeMap::<String, RawPayloadRef>::new();
    let mut inference_contexts = BTreeMap::<String, (String, String)>::new();
    let mut inference_terminals = BTreeSet::new();
    let mut rollout_ended = false;
    // split_terminator removes only the trailing delimiter, preserving corrupt
    // internal blank records that the append-only writer never emits.
    let event_log = std::str::from_utf8(&event_log_jsonl)
        .map_err(|_| anyhow!("trace event log must be UTF-8"))?;
    for line in event_log.split_terminator('\n') {
        ensure!(
            line.len() < MAX_RECORD_BYTES,
            "trace event exceeds the record byte limit including newline"
        );
        ensure!(
            !line.trim().is_empty(),
            "trace event log contains a blank record"
        );
        ensure!(
            (events.len() as u64) < MAX_EVENT_COUNT,
            "trace exceeds the event count limit"
        );
        let event: RawTraceEvent = serde_json::from_str(line)
            .map_err(|_| anyhow!("invalid trace event JSON at ordinal {}", events.len() + 1))?;
        ensure!(
            event.schema_version == RAW_TRACE_EVENT_SCHEMA_VERSION,
            "unsupported trace event schema"
        );
        ensure!(
            event.seq == events.len() as u64 + 1,
            "trace event sequence must be contiguous from one"
        );
        ensure!(
            event.rollout_id == manifest.rollout_id,
            "trace event rollout identity disagrees with manifest"
        );
        match &event.payload {
            RawTraceEventPayload::RolloutStarted {
                trace_id,
                root_thread_id,
            } => {
                ensure!(
                    event.seq == 1,
                    "rollout start must be the first trace event"
                );
                ensure!(
                    *trace_id == manifest.trace_id && *root_thread_id == manifest.root_thread_id,
                    "rollout start identity disagrees with manifest"
                );
            }
            RawTraceEventPayload::RolloutEnded { .. } => {
                ensure!(!rollout_ended, "duplicate rollout terminal event");
                rollout_ended = true;
            }
            RawTraceEventPayload::InferenceStarted {
                inference_call_id,
                thread_id,
                codex_turn_id,
                ..
            } => {
                ensure!(
                    !inference_contexts.contains_key(inference_call_id),
                    "duplicate inference start"
                );
                validate_context(&event, thread_id, codex_turn_id)?;
                inference_contexts.insert(
                    inference_call_id.clone(),
                    (thread_id.clone(), codex_turn_id.clone()),
                );
            }
            RawTraceEventPayload::InferenceCompleted {
                inference_call_id, ..
            }
            | RawTraceEventPayload::InferenceFailed {
                inference_call_id, ..
            }
            | RawTraceEventPayload::InferenceCancelled {
                inference_call_id, ..
            } => {
                let (thread_id, turn_id) = inference_contexts
                    .get(inference_call_id)
                    .context("inference terminal referenced an unknown call")?;
                validate_context(&event, thread_id, turn_id)?;
                ensure!(
                    inference_terminals.insert(inference_call_id.clone()),
                    "duplicate inference terminal event"
                );
            }
            // Domain lifecycle validation remains in the existing reducer. A
            // missing terminal is valid partial evidence, never provider success.
            _ => {}
        }
        for reference in event.payload.raw_payload_refs() {
            let ordinal = reference
                .raw_payload_id
                .strip_prefix("raw_payload:")
                .and_then(|ordinal| ordinal.parse::<u64>().ok())
                .filter(|ordinal| *ordinal > 0 && *ordinal <= MAX_PAYLOAD_COUNT)
                .context("invalid raw payload identity")?;
            ensure!(
                reference.raw_payload_id == format!("raw_payload:{ordinal}")
                    && reference.path == format!("{PAYLOADS_DIR_NAME}/{ordinal}.json"),
                "raw payload identity and path disagree"
            );
            if let Some(previous) = references.get(&reference.raw_payload_id) {
                ensure!(
                    previous == reference,
                    "raw payload identity was reused with a different reference"
                );
            } else {
                // Canonical ordinals in 1..=MAX_PAYLOAD_COUNT bound the unique
                // reference count as well as the producer's namespace.
                references.insert(reference.raw_payload_id.clone(), reference.clone());
            }
        }
        events.push(event);
    }
    let mut payloads = BTreeMap::new();
    for (id, reference) in references {
        let bytes = read_file(
            &bundle_dir.join(reference.path),
            &payloads_dir,
            MAX_RECORD_BYTES,
            &mut remaining,
        )
        .with_context(|| format!("read {id}"))?;
        // Parse all payloads, including protocol breadcrumbs ignored by the
        // semantic reducer. Retain original bytes for provenance and redaction.
        // IgnoredAny skips unneeded string contents without validating UTF-8.
        // Check the complete bytes even when the reducer ignores this payload.
        let text = std::str::from_utf8(&bytes).map_err(|_| anyhow!("invalid UTF-8 in {id}"))?;
        serde_json::from_str::<IgnoredAny>(text).map_err(|_| anyhow!("invalid JSON in {id}"))?;
        payloads.insert(id, bytes);
    }
    ensure!(
        same_file(&manifest_stamp, &fs::symlink_metadata(manifest_path)?)
            && same_file(&event_log_stamp, &fs::symlink_metadata(event_log_path)?),
        "trace manifest or event log changed during snapshot admission"
    );
    Ok(BundleSnapshot {
        manifest,
        manifest_json,
        event_log_jsonl,
        events,
        payloads,
    })
}

fn validate_context(event: &RawTraceEvent, thread_id: &str, turn_id: &str) -> Result<()> {
    ensure!(
        event.thread_id.as_deref().is_none_or(|id| id == thread_id),
        "inference envelope thread disagrees with its lifecycle"
    );
    ensure!(
        event
            .codex_turn_id
            .as_deref()
            .is_none_or(|id| id == turn_id),
        "inference envelope turn disagrees with its lifecycle"
    );
    Ok(())
}

fn read_file(
    path: &Path,
    directory: &Path,
    limit: usize,
    remaining: &mut usize,
) -> Result<Vec<u8>> {
    validate_private_dir(directory)?;
    let before = fs::symlink_metadata(path)?;
    ensure!(before.is_file(), "trace file must not be linked or special");
    let limit = limit.min(*remaining);
    ensure!(before.len() <= limit as u64, "trace exceeds its byte limit");
    let file = File::open(path)?;
    validate_private_file(&file, directory)?;
    ensure!(
        same_file(&before, &file.metadata()?),
        "trace file changed before admission"
    );
    let mut bytes = Vec::new();
    (&file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "trace exceeds its byte limit");
    validate_private_file(&file, directory)?;
    ensure!(
        same_file(&before, &file.metadata()?) && same_file(&before, &fs::symlink_metadata(path)?),
        "trace file changed during admission"
    );
    ensure!(
        bytes.len() as u64 == before.len(),
        "trace file changed during admission"
    );
    *remaining -= bytes.len();
    Ok(bytes)
}

fn same_file(before: &Metadata, after: &Metadata) -> bool {
    let common = after.is_file()
        && before.len() == after.len()
        && before.modified().ok() == after.modified().ok();
    #[cfg(unix)]
    {
        common
            && before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        common && before.created().ok() == after.created().ok()
    }
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
