//! Hot-path trace bundle writer.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Serialize;

use crate::bundle::MANIFEST_FILE_NAME;
use crate::bundle::PAYLOADS_DIR_NAME;
use crate::bundle::RAW_EVENT_LOG_FILE_NAME;
use crate::bundle::TraceBundleManifest;
use crate::limits::MAX_BUNDLE_BYTES;
use crate::limits::MAX_EVENT_COUNT;
use crate::limits::MAX_PAYLOAD_COUNT;
use crate::limits::MAX_RECORD_BYTES;
use crate::model::AgentThreadId;
use crate::payload::RawPayloadKind;
use crate::payload::RawPayloadRef;
use crate::raw_event::RAW_TRACE_EVENT_SCHEMA_VERSION;
use crate::raw_event::RawTraceEvent;
use crate::raw_event::RawTraceEventContext;
use crate::raw_event::RawTraceEventPayload;
use crate::storage::validate_private_dir;
use crate::storage::validate_private_file;

/// Local trace bundle writer.
///
/// The writer appends raw events and writes payload files. It does not keep a
/// reduced `RolloutTrace` in memory; replay is owned by the reducer.
#[derive(Debug)]
pub struct TraceWriter {
    inner: Mutex<TraceWriterInner>,
}

#[derive(Debug)]
struct TraceWriterInner {
    manifest: TraceBundleManifest,
    payloads_dir: PathBuf,
    event_log: File,
    next_seq: u64,
    next_payload_ordinal: u64,
    remaining_bytes: usize,
    failed: bool,
}

impl TraceWriter {
    /// Creates private capture files without replacing an existing bundle.
    ///
    /// A write failure permanently stops this writer. Later events must not
    /// make a capture with missing evidence appear complete. Flush acknowledges
    /// visibility only; this diagnostic writer does not promise crash durability.
    pub fn create(
        bundle_dir: impl AsRef<Path>,
        trace_id: String,
        rollout_id: String,
        root_thread_id: AgentThreadId,
    ) -> Result<Self> {
        let bundle_dir = bundle_dir.as_ref().to_path_buf();
        let payloads_dir = bundle_dir.join(PAYLOADS_DIR_NAME);
        create_private_dir(&bundle_dir)?;
        if std::fs::read_dir(&bundle_dir)
            .context("inspect trace directory")?
            .next()
            .transpose()
            .context("inspect trace directory entry")?
            .is_some()
        {
            bail!("trace directory must be empty; existing evidence was preserved");
        }
        create_private_dir(&payloads_dir)?;

        let started_at_unix_ms = unix_time_ms();
        let manifest =
            TraceBundleManifest::new(trace_id, rollout_id, root_thread_id, started_at_unix_ms);
        let manifest_bytes = encode_record(&manifest, JsonLayout::Pretty, MAX_RECORD_BYTES)?;
        let mut manifest_file =
            create_private_file(&bundle_dir.join(MANIFEST_FILE_NAME), &bundle_dir)?;
        manifest_file
            .write_all(&manifest_bytes)
            .context("write trace manifest")?;

        let event_log_path = bundle_dir.join(RAW_EVENT_LOG_FILE_NAME);
        let event_log = create_private_file(&event_log_path, &bundle_dir)?;

        Ok(Self {
            inner: Mutex::new(TraceWriterInner {
                manifest,
                payloads_dir,
                event_log,
                next_seq: 1,
                next_payload_ordinal: 1,
                remaining_bytes: MAX_BUNDLE_BYTES - manifest_bytes.len(),
                failed: false,
            }),
        })
    }

    /// Writes a JSON payload file and returns its reduced-state reference.
    pub fn write_json_payload(
        &self,
        kind: RawPayloadKind,
        value: &impl Serialize,
    ) -> Result<RawPayloadRef> {
        self.record(|inner| {
            let ordinal = inner.next_payload_ordinal;
            if ordinal > MAX_PAYLOAD_COUNT {
                bail!("trace payload count limit reached; capture stopped");
            }
            let bytes = encode_record(
                value,
                JsonLayout::Pretty,
                inner.remaining_bytes.min(MAX_RECORD_BYTES),
            )?;
            let absolute_path = inner.payloads_dir.join(format!("{ordinal}.json"));
            let mut file = create_private_file(&absolute_path, &inner.payloads_dir)?;
            // Reserve before writing, including any partial or orphaned file.
            // A reference is returned only after the complete payload is visible.
            inner.remaining_bytes -= bytes.len();
            file.write_all(&bytes).context("write trace payload")?;
            inner.next_payload_ordinal += 1;
            Ok(RawPayloadRef {
                raw_payload_id: format!("raw_payload:{ordinal}"),
                kind,
                path: format!("{PAYLOADS_DIR_NAME}/{ordinal}.json"),
            })
        })
    }

    /// Appends one raw event with no extra envelope context.
    pub fn append(&self, payload: RawTraceEventPayload) -> Result<RawTraceEvent> {
        self.append_with_context(RawTraceEventContext::default(), payload)
    }

    /// Appends one raw event with explicit thread/turn context.
    pub fn append_with_context(
        &self,
        context: RawTraceEventContext,
        payload: RawTraceEventPayload,
    ) -> Result<RawTraceEvent> {
        self.record(|inner| {
            if inner.next_seq > MAX_EVENT_COUNT {
                bail!("trace event count limit reached; capture stopped");
            }
            let event = RawTraceEvent {
                schema_version: RAW_TRACE_EVENT_SCHEMA_VERSION,
                seq: inner.next_seq,
                wall_time_unix_ms: unix_time_ms(),
                rollout_id: inner.manifest.rollout_id.clone(),
                thread_id: context.thread_id,
                codex_turn_id: context.codex_turn_id,
                payload,
            };
            let bytes = encode_record(
                &event,
                JsonLayout::CompactLine,
                inner.remaining_bytes.min(MAX_RECORD_BYTES),
            )?;
            inner.remaining_bytes -= bytes.len();
            inner
                .event_log
                .write_all(&bytes)
                .context("write trace event")?;
            inner.event_log.flush().context("flush trace event")?;
            inner.next_seq += 1;
            Ok(event)
        })
    }

    fn record<T>(&self, write: impl FnOnce(&mut TraceWriterInner) -> Result<T>) -> Result<T> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("trace writer stopped after interrupted write"))?;
        if inner.failed {
            bail!("trace writer stopped after earlier capture failure");
        }
        let result = write(&mut inner);
        if result.is_err() {
            inner.failed = true;
        }
        result
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .context("create private trace directory")?;
    validate_private_dir(path)
}

fn create_private_file(path: &Path, directory: &Path) -> Result<File> {
    validate_private_dir(directory)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .context("create new private trace file")?;
    validate_private_file(&file, directory)?;
    Ok(file)
}

pub(crate) enum JsonLayout {
    Pretty,
    CompactLine,
}

struct RecordBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for RecordBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other(
                "trace record byte limit reached; not truncated",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn encode_record(
    value: &impl Serialize,
    layout: JsonLayout,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut buffer = RecordBuffer {
        bytes: Vec::new(),
        limit,
    };
    match layout {
        JsonLayout::Pretty => serde_json::to_writer_pretty(&mut buffer, value).map_err(|_| {
            anyhow::anyhow!("trace record encoding failed or exceeded its byte limit")
        })?,
        JsonLayout::CompactLine => {
            serde_json::to_writer(&mut buffer, value).map_err(|_| {
                anyhow::anyhow!("trace record encoding failed or exceeded its byte limit")
            })?;
            buffer.write_all(b"\n")?;
        }
    }
    Ok(buffer.bytes)
}

pub(crate) fn unix_time_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::fs;

    use crate::model::ExecutionStatus;
    use crate::model::RolloutStatus;
    use crate::payload::RawPayloadKind;
    use crate::raw_event::RawTraceEventPayload;
    use crate::reducer::test_support::private_tempdir;
    use crate::replay_bundle;
    use crate::writer::TraceWriter;

    #[test]
    fn an_existing_bundle_is_never_reopened_or_replaced() -> anyhow::Result<()> {
        let temp = private_tempdir()?;
        let writer = TraceWriter::create(
            temp.path(),
            "trace-1".into(),
            "rollout-1".into(),
            "root".into(),
        )?;
        writer.append(RawTraceEventPayload::RolloutStarted {
            trace_id: "trace-1".into(),
            root_thread_id: "root".into(),
        })?;
        drop(writer);
        let manifest = fs::read(temp.path().join("manifest.json"))?;
        let events = fs::read(temp.path().join("trace.jsonl"))?;
        assert!(
            TraceWriter::create(
                temp.path(),
                "different".into(),
                "other".into(),
                "root".into()
            )
            .is_err()
        );
        assert_eq!(fs::read(temp.path().join("manifest.json"))?, manifest);
        assert_eq!(fs::read(temp.path().join("trace.jsonl"))?, events);
        Ok(())
    }

    #[test]
    fn payload_collision_preserves_evidence_and_stops_later_events() -> anyhow::Result<()> {
        let temp = private_tempdir()?;
        let writer = TraceWriter::create(
            temp.path(),
            "trace-1".into(),
            "rollout-1".into(),
            "root".into(),
        )?;
        let path = temp.path().join("payloads/1.json");
        fs::write(&path, b"original")?;
        assert!(
            writer
                .write_json_payload(RawPayloadKind::ToolResult, &json!({"replacement":true}))
                .is_err()
        );
        assert_eq!(fs::read(path)?, b"original");
        assert!(
            writer
                .append(RawTraceEventPayload::RolloutStarted {
                    trace_id: "trace-1".into(),
                    root_thread_id: "root".into(),
                })
                .is_err()
        );
        assert!(fs::read(temp.path().join("trace.jsonl"))?.is_empty());
        Ok(())
    }

    #[test]
    fn a_partial_bundle_is_preserved_without_adding_a_manifest() -> anyhow::Result<()> {
        let temp = private_tempdir()?;
        fs::create_dir(temp.path().join("payloads"))?;
        fs::write(temp.path().join("payloads/1.json"), b"prior evidence")?;
        assert!(
            TraceWriter::create(
                temp.path(),
                "trace-1".into(),
                "rollout-1".into(),
                "root".into()
            )
            .is_err()
        );
        assert!(!temp.path().join("manifest.json").exists());
        assert!(!temp.path().join("trace.jsonl").exists());
        assert_eq!(
            fs::read(temp.path().join("payloads/1.json"))?,
            b"prior evidence"
        );
        Ok(())
    }

    #[test]
    fn count_exhaustion_and_io_failure_stop_all_later_capture() -> anyhow::Result<()> {
        #[derive(Clone, Copy)]
        enum Boundary {
            EventCount,
            PayloadCount,
            WriteFailure,
        }
        for boundary in [
            Boundary::EventCount,
            Boundary::PayloadCount,
            Boundary::WriteFailure,
        ] {
            let temp = private_tempdir()?;
            let writer = TraceWriter::create(
                temp.path(),
                "trace-1".into(),
                "rollout-1".into(),
                "root".into(),
            )?;
            {
                let mut inner = writer
                    .inner
                    .lock()
                    .map_err(|_| anyhow::anyhow!("test lock"))?;
                match boundary {
                    Boundary::EventCount => inner.next_seq = crate::limits::MAX_EVENT_COUNT + 1,
                    Boundary::PayloadCount => {
                        inner.next_payload_ordinal = crate::limits::MAX_PAYLOAD_COUNT + 1
                    }
                    Boundary::WriteFailure => {
                        inner.event_log = fs::File::open(temp.path().join("trace.jsonl"))?
                    }
                }
            }
            if matches!(boundary, Boundary::PayloadCount) {
                assert!(
                    writer
                        .write_json_payload(RawPayloadKind::ToolResult, &json!("value"))
                        .is_err()
                );
            } else {
                assert!(
                    writer
                        .append(RawTraceEventPayload::RolloutStarted {
                            trace_id: "trace-1".into(),
                            root_thread_id: "root".into(),
                        })
                        .is_err()
                );
            }
            assert!(
                writer
                    .write_json_payload(RawPayloadKind::ToolResult, &json!("later"))
                    .is_err()
            );
            assert!(fs::read(temp.path().join("trace.jsonl"))?.is_empty());
            assert!(!temp.path().join("payloads/1.json").exists());
        }
        Ok(())
    }

    #[test]
    fn event_newline_is_part_of_the_whole_record_budget() -> anyhow::Result<()> {
        use super::JsonLayout;
        use super::encode_record;
        assert_eq!(
            encode_record(&json!("α"), JsonLayout::CompactLine, /*limit*/ 5)?,
            "\"α\"\n".as_bytes()
        );
        assert!(encode_record(&json!("α"), JsonLayout::CompactLine, /*limit*/ 4).is_err());
        Ok(())
    }

    #[test]
    fn bundle_budget_counts_whole_utf8_records_and_never_truncates() -> anyhow::Result<()> {
        let temp = private_tempdir()?;
        let writer = TraceWriter::create(
            temp.path(),
            "trace-1".into(),
            "rollout-1".into(),
            "root".into(),
        )?;
        writer
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("test lock"))?
            .remaining_bytes = 5;
        let reference = writer.write_json_payload(RawPayloadKind::ToolResult, &json!("α"))?;
        assert_eq!(
            fs::read(temp.path().join(reference.path))?,
            "\"α\"".as_bytes()
        );
        assert!(
            writer
                .write_json_payload(RawPayloadKind::ToolResult, &json!(null))
                .is_err()
        );
        assert!(!temp.path().join("payloads/2.json").exists());
        assert!(
            writer
                .write_json_payload(RawPayloadKind::ToolResult, &json!(0))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn oversized_record_rejects_before_creating_a_partial_payload() -> anyhow::Result<()> {
        let temp = private_tempdir()?;
        let writer = TraceWriter::create(
            temp.path(),
            "trace-1".into(),
            "rollout-1".into(),
            "root".into(),
        )?;
        let content = "x".repeat(crate::limits::MAX_RECORD_BYTES);
        assert!(
            writer
                .write_json_payload(RawPayloadKind::InferenceRequest, &content)
                .is_err()
        );
        assert!(!temp.path().join("payloads/1.json").exists());
        assert!(fs::read(temp.path().join("trace.jsonl"))?.is_empty());
        Ok(())
    }

    #[test]
    fn serialization_failure_cannot_leak_content_or_resume_a_broken_capture() -> anyhow::Result<()>
    {
        struct InvalidPayload;
        impl serde::Serialize for InvalidPayload {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("PRIVATE-SERIALIZATION-SENTINEL"))
            }
        }
        let temp = private_tempdir()?;
        let writer = TraceWriter::create(
            temp.path(),
            "trace-1".into(),
            "rollout-1".into(),
            "root".into(),
        )?;
        let failure = writer.write_json_payload(RawPayloadKind::ToolResult, &InvalidPayload);
        assert!(failure.is_err());
        assert!(!format!("{failure:?}").contains("PRIVATE-SERIALIZATION-SENTINEL"));
        assert!(
            writer
                .write_json_payload(RawPayloadKind::ToolResult, &json!("later"))
                .is_err()
        );
        assert!(!temp.path().join("payloads/1.json").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn new_files_are_private_and_shared_existing_directories_are_rejected() -> anyhow::Result<()> {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let temp = private_tempdir()?;
        let path = temp.path().join("private");
        let writer =
            TraceWriter::create(&path, "trace-1".into(), "rollout-1".into(), "root".into())?;
        let reference = writer.write_json_payload(RawPayloadKind::ToolResult, &json!("value"))?;
        for directory in [&path, &path.join("payloads")] {
            assert_eq!(fs::metadata(directory)?.mode() & 0o777, 0o700);
        }
        for file in [
            path.join("manifest.json"),
            path.join("trace.jsonl"),
            path.join(reference.path),
        ] {
            assert_eq!(fs::metadata(file)?.mode() & 0o777, 0o600);
        }
        let shared = temp.path().join("shared");
        fs::create_dir(&shared)?;
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755))?;
        assert!(
            TraceWriter::create(&shared, "trace-2".into(), "rollout-2".into(), "root".into())
                .is_err()
        );
        assert!(!shared.join("manifest.json").exists());
        Ok(())
    }

    #[test]
    fn writer_records_payload_refs_and_replays_rollout_status() -> anyhow::Result<()> {
        let temp = private_tempdir()?;
        let writer = TraceWriter::create(
            temp.path(),
            "trace-1".to_string(),
            "rollout-1".to_string(),
            "thread-root".to_string(),
        )?;

        writer.append(RawTraceEventPayload::RolloutStarted {
            trace_id: "trace-1".to_string(),
            root_thread_id: "thread-root".to_string(),
        })?;
        let metadata_payload = writer.write_json_payload(
            RawPayloadKind::ProtocolEvent,
            &json!({
                "source": "test",
                "model": "gpt-test",
            }),
        )?;
        writer.append(RawTraceEventPayload::ThreadStarted {
            thread_id: "thread-root".to_string(),
            agent_path: "/root".to_string(),
            metadata_payload: Some(metadata_payload.clone()),
        })?;
        writer.append(RawTraceEventPayload::CodexTurnStarted {
            codex_turn_id: "turn-1".to_string(),
            thread_id: "thread-root".to_string(),
        })?;
        let inference_request = writer.write_json_payload(
            RawPayloadKind::InferenceRequest,
            &json!({
                "model": "gpt-test",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}]
                }],
            }),
        )?;
        writer.append(RawTraceEventPayload::InferenceStarted {
            inference_call_id: "inference-1".to_string(),
            thread_id: "thread-root".to_string(),
            codex_turn_id: "turn-1".to_string(),
            model: "gpt-test".to_string(),
            provider_name: "test-provider".to_string(),
            request_payload: inference_request.clone(),
        })?;
        let inference_response = writer.write_json_payload(
            RawPayloadKind::InferenceResponse,
            &json!({
                "response_id": "resp-1",
                "output_items": [],
            }),
        )?;
        writer.append(RawTraceEventPayload::InferenceCompleted {
            inference_call_id: "inference-1".to_string(),
            response_id: Some("resp-1".to_string()),
            upstream_request_id: Some("req-1".to_string()),
            response_payload: inference_response.clone(),
        })?;
        writer.append(RawTraceEventPayload::CodexTurnEnded {
            codex_turn_id: "turn-1".to_string(),
            status: ExecutionStatus::Completed,
        })?;
        writer.append(RawTraceEventPayload::RolloutEnded {
            status: RolloutStatus::Completed,
        })?;

        let rollout = replay_bundle(temp.path())?;

        assert_eq!(rollout.status, RolloutStatus::Completed);
        assert_eq!(rollout.root_thread_id, "thread-root");
        assert_eq!(rollout.threads["thread-root"].agent_path, "/root");
        assert_eq!(rollout.codex_turns["turn-1"].thread_id, "thread-root");
        assert_eq!(
            rollout.codex_turns["turn-1"].execution.status,
            ExecutionStatus::Completed,
        );
        assert_eq!(
            rollout.inference_calls["inference-1"].raw_request_payload_id,
            inference_request.raw_payload_id,
        );
        assert_eq!(
            rollout.inference_calls["inference-1"].raw_response_payload_id,
            Some(inference_response.raw_payload_id),
        );
        assert_eq!(
            rollout.raw_payloads[&metadata_payload.raw_payload_id].path,
            "payloads/1.json"
        );

        Ok(())
    }
}
