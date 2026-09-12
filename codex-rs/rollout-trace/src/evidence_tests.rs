use std::fs;
use std::io;
use std::io::BufRead;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::write_evidence;
use super::write_snapshot;
use crate::admit_bundle;
use crate::bundle::MANIFEST_FILE_NAME;
use crate::bundle::RAW_EVENT_LOG_FILE_NAME;
use crate::limits::MAX_BUNDLE_BYTES;
use crate::limits::MAX_RECORD_BYTES;
use crate::model::ExecutionStatus;
use crate::payload::RawPayloadKind;
use crate::raw_event::RawTraceEventPayload;
use crate::reducer::test_support::append_inference_start;
use crate::reducer::test_support::create_started_writer;
use crate::reducer::test_support::private_tempdir;
use crate::reducer::test_support::start_turn;

struct Decoded {
    header: Value,
    frames: Vec<(Value, Vec<u8>)>,
    footer: Value,
}

fn read_descriptor(input: &mut Cursor<&[u8]>) -> Result<Value> {
    let mut line = Vec::new();
    input.take(/*limit*/ 1025).read_until(b'\n', &mut line)?;
    ensure!(
        line.len() <= 1024 && line.ends_with(b"\n"),
        "descriptor framing"
    );
    Ok(serde_json::from_slice(&line)?)
}

fn decode(bytes: &[u8]) -> Result<Decoded> {
    let mut input = Cursor::new(bytes);
    let header = read_descriptor(&mut input)?;
    ensure!(header["kind"] == "header", "header kind");
    let count = header["data_records"]
        .as_u64()
        .context("data record count")?;
    ensure!(count <= 4099, "too many data records");
    let mut frames = Vec::new();
    for _ in 0..count {
        let descriptor = read_descriptor(&mut input)?;
        let size = descriptor["bytes"].as_u64().context("frame byte count")?;
        ensure!(size <= bytes.len() as u64, "frame exceeds fixture");
        let mut body = vec![0; size as usize];
        input.read_exact(&mut body)?;
        let mut delimiter = [0];
        input.read_exact(&mut delimiter)?;
        ensure!(delimiter == [b'\n'], "body delimiter");
        frames.push((descriptor, body));
    }
    let footer = read_descriptor(&mut input)?;
    ensure!(footer["kind"] == "end", "footer kind");
    ensure!(
        input.position() == bytes.len() as u64,
        "unexpected trailing bytes"
    );
    Ok(Decoded {
        header,
        frames,
        footer,
    })
}

#[test]
fn exact_frames_preserve_source_bytes_and_noncontiguous_numeric_ordinals() -> Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    let mut references = Vec::new();
    for ordinal in 1..=12 {
        references.push(writer.write_json_payload(
            RawPayloadKind::ProtocolEvent,
            &json!({"source":"synthetic", "ordinal":ordinal}),
        )?);
    }
    for index in [9, 1, 11, 9] {
        writer.append(RawTraceEventPayload::ProtocolEventObserved {
            event_type: "synthetic".to_string(),
            event_payload: references[index].clone(),
        })?;
    }
    let exact = " { \"line\" : \"a\\nb\", \"雪\" : \"🙂\" } \n\t".as_bytes();
    fs::write(temp.path().join(&references[9].path), exact)?;
    let snapshot = admit_bundle(temp.path())?;
    let mut wire = Vec::new();
    write_evidence(temp.path(), &mut wire)?;
    let decoded = decode(&wire)?;
    assert_eq!(decoded.frames.len(), 6);
    assert_eq!(decoded.frames[0].0["kind"], "manifest");
    assert_eq!(decoded.frames[0].1, snapshot.manifest_json);
    assert_eq!(decoded.frames[1].0["kind"], "events");
    assert_eq!(decoded.frames[1].1, snapshot.event_log_jsonl);
    for (index, ordinal) in [2, 10, 12].into_iter().enumerate() {
        let (descriptor, bytes) = &decoded.frames[index + 2];
        assert_eq!(
            descriptor,
            &json!({"kind":"payload","ordinal":ordinal,"bytes":bytes.len()})
        );
        assert_eq!(bytes, &snapshot.payloads[&format!("raw_payload:{ordinal}")]);
    }
    let (graph_descriptor, graph) = &decoded.frames[5];
    assert_eq!(graph_descriptor["kind"], "graph");
    assert_eq!(
        serde_json::from_slice::<Value>(graph)?,
        serde_json::to_value(&snapshot.rollout)?
    );
    assert!(graph.ends_with(b"\n"));
    let source_bytes: usize = decoded.frames[..5].iter().map(|(_, body)| body.len()).sum();
    assert_eq!(
        decoded.header,
        json!({"kind":"header","format":"codex_trace_evidence","schema_version":1,
            "source_bytes":source_bytes,"graph_bytes":graph.len(),
            "event_count":snapshot.events.len(),"payload_count":3,"data_records":6})
    );
    assert_eq!(
        decoded.footer,
        json!({"kind":"end","source_bytes":source_bytes,"graph_bytes":graph.len(),
            "event_count":snapshot.events.len(),"payload_count":3,"data_records":6})
    );
    Ok(())
}

#[test]
fn projection_never_reopens_admitted_source_files() -> Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    let payload = writer.write_json_payload(RawPayloadKind::ProtocolEvent, &json!([1, 2, 3]))?;
    writer.append(RawTraceEventPayload::ProtocolEventObserved {
        event_type: "synthetic".to_string(),
        event_payload: payload.clone(),
    })?;
    drop(writer);
    let snapshot = admit_bundle(temp.path())?;
    for path in [MANIFEST_FILE_NAME, RAW_EVENT_LOG_FILE_NAME, &payload.path] {
        fs::remove_file(temp.path().join(path))?;
    }
    let mut wire = Vec::new();
    write_snapshot(&snapshot, &mut wire, MAX_BUNDLE_BYTES)?;
    assert_eq!(
        decode(&wire)?.frames[2].1,
        snapshot.payloads[&payload.raw_payload_id]
    );
    Ok(())
}

#[test]
fn raw_terminal_absence_survives_inferred_graph_closure() -> Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;
    let request = writer.write_json_payload(
        RawPayloadKind::InferenceRequest,
        &json!({"input":[],"instructions":"full captured instructions","tools":[]}),
    )?;
    append_inference_start(&writer, "attempt-1", "turn-1", request)?;
    writer.append(RawTraceEventPayload::CodexTurnEnded {
        codex_turn_id: "turn-1".to_string(),
        status: ExecutionStatus::Completed,
    })?;
    let mut wire = Vec::new();
    write_evidence(temp.path(), &mut wire)?;
    let decoded = decode(&wire)?;
    let events: Vec<Value> = std::str::from_utf8(&decoded.frames[1].1)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(events.len(), 4);
    assert_eq!(events[2]["payload"]["type"], "inference_started");
    assert_eq!(events[3]["payload"]["type"], "codex_turn_ended");
    let graph: Value = serde_json::from_slice(&decoded.frames.last().unwrap().1)?;
    assert_eq!(
        graph["inference_calls"]["attempt-1"]["execution"]["status"],
        "cancelled"
    );
    assert!(graph["inference_calls"]["attempt-1"]["raw_response_payload_id"].is_null());
    assert!(decoded.footer.get("capture_complete").is_none());
    Ok(())
}

#[test]
fn admission_and_graph_encoding_fail_before_the_header() -> Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    drop(writer);
    let snapshot = admit_bundle(temp.path())?;
    let mut wire = Vec::new();
    assert!(write_snapshot(&snapshot, &mut wire, /*graph_limit*/ 1).is_err());
    assert!(wire.is_empty());
    fs::write(temp.path().join(RAW_EVENT_LOG_FILE_NAME), b"{ incomplete")?;
    assert!(write_evidence(temp.path(), &mut wire).is_err());
    assert!(wire.is_empty());
    Ok(())
}

struct FailingWriter {
    bytes: Vec<u8>,
    remaining: usize,
    fail_flush: bool,
}

impl Write for FailingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::other("SYNTHETIC_PRIVATE_IO_CANARY"));
        }
        let count = bytes.len().min(self.remaining);
        self.bytes.extend_from_slice(&bytes[..count]);
        self.remaining -= count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.fail_flush {
            Err(io::Error::other("SYNTHETIC_PRIVATE_IO_CANARY"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn incomplete_writes_withhold_a_valid_footer_and_private_io_errors() -> Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    drop(writer);
    let snapshot = admit_bundle(temp.path())?;
    let mut complete = Vec::new();
    write_snapshot(&snapshot, &mut complete, MAX_BUNDLE_BYTES)?;
    for remaining in [0, 1, 100, complete.len() / 2, complete.len() - 1] {
        let mut sink = FailingWriter {
            bytes: Vec::new(),
            remaining,
            fail_flush: false,
        };
        let error = write_snapshot(&snapshot, &mut sink, MAX_BUNDLE_BYTES).unwrap_err();
        assert!(!format!("{error:#}").contains("SYNTHETIC_PRIVATE_IO_CANARY"));
        assert!(decode(&sink.bytes).is_err());
    }
    let mut sink = FailingWriter {
        bytes: Vec::new(),
        remaining: usize::MAX,
        fail_flush: true,
    };
    let error = write_snapshot(&snapshot, &mut sink, MAX_BUNDLE_BYTES).unwrap_err();
    assert_eq!(error.to_string(), "trace evidence flush failed");
    // Complete bytes alone do not acknowledge writer completion.
    assert!(decode(&sink.bytes).is_ok());
    Ok(())
}

#[test]
fn maximum_payload_is_carried_without_json_string_expansion() -> Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    let payload = writer.write_json_payload(RawPayloadKind::ProtocolEvent, &json!("synthetic"))?;
    writer.append(RawTraceEventPayload::ProtocolEventObserved {
        event_type: "synthetic".to_string(),
        event_payload: payload.clone(),
    })?;
    drop(writer);
    let mut raw = vec![b'\\'; MAX_RECORD_BYTES];
    raw[0] = b'"';
    raw[MAX_RECORD_BYTES - 1] = b'"';
    fs::write(temp.path().join(payload.path), &raw)?;
    let mut wire = Vec::new();
    write_evidence(temp.path(), &mut wire)?;
    let decoded = decode(&wire)?;
    assert_eq!(decoded.frames[2].1, raw);
    assert!(wire.len() < MAX_RECORD_BYTES + 16_384);
    Ok(())
}
