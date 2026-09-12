use std::fs;
use std::fs::OpenOptions;

use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

use super::read_file;
use crate::admit_bundle;
use crate::bundle::MANIFEST_FILE_NAME;
use crate::bundle::RAW_EVENT_LOG_FILE_NAME;
use crate::limits::MAX_BUNDLE_BYTES;
use crate::limits::MAX_EVENT_COUNT;
use crate::limits::MAX_PAYLOAD_COUNT;
use crate::limits::MAX_RECORD_BYTES;
use crate::model::ExecutionStatus;
use crate::payload::RawPayloadKind;
use crate::payload::RawPayloadRef;
use crate::raw_event::RawTraceEventPayload;
use crate::reducer::test_support::append_inference_start;
use crate::reducer::test_support::create_started_writer;
use crate::reducer::test_support::private_tempdir;
use crate::reducer::test_support::start_turn;
use crate::replay_bundle;

fn protocol_bundle() -> Result<(TempDir, RawPayloadRef)> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    let reference = writer.write_json_payload(
        RawPayloadKind::ProtocolEvent,
        &json!({"source": "synthetic"}),
    )?;
    for _ in 0..2 {
        writer.append(RawTraceEventPayload::ProtocolEventObserved {
            event_type: "synthetic".to_string(),
            event_payload: reference.clone(),
        })?;
    }
    Ok((temp, reference))
}

fn read_events(temp: &TempDir) -> Result<Vec<Value>> {
    fs::read_to_string(temp.path().join(RAW_EVENT_LOG_FILE_NAME))?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(Into::into)
}

fn write_events(temp: &TempDir, events: &[Value]) -> Result<()> {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event)?;
        bytes.push(b'\n');
    }
    fs::write(temp.path().join(RAW_EVENT_LOG_FILE_NAME), bytes)?;
    Ok(())
}

fn expect_failure(temp: &TempDir, expected: &str) {
    let error = match admit_bundle(temp.path()) {
        Ok(_) => panic!("expected admission failure containing {expected}"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains(expected),
        "unexpected admission error: {error}"
    );
}

#[test]
fn exact_bytes_and_repeated_reference_survive_admission() -> Result<()> {
    let (temp, reference) = protocol_bundle()?;
    let payload_bytes = b" { \"source\" : \"synthetic\", \"tail\" : [1, 2] } \n";
    fs::write(temp.path().join(&reference.path), payload_bytes)?;
    let manifest_bytes = fs::read(temp.path().join(MANIFEST_FILE_NAME))?;
    let log_bytes = fs::read(temp.path().join(RAW_EVENT_LOG_FILE_NAME))?;
    let admitted = admit_bundle(temp.path())?;
    assert_eq!(&admitted.rollout, &replay_bundle(temp.path())?);
    assert_eq!(
        (
            admitted.manifest_json,
            admitted.event_log_jsonl,
            admitted.payloads
        ),
        (
            manifest_bytes,
            log_bytes,
            [(reference.raw_payload_id, payload_bytes.to_vec())].into()
        ),
    );
    assert_eq!(admitted.events.len(), 3);
    Ok(())
}

#[test]
fn manifest_schema_layout_and_identity_are_admitted() -> Result<()> {
    for (field, replacement, expected) in [
        (
            "schema_version",
            json!(2),
            "unsupported trace manifest schema",
        ),
        (
            "raw_event_log",
            json!("alternate.jsonl"),
            "unsupported trace bundle layout",
        ),
        (
            "payloads_dir",
            json!("alternate"),
            "unsupported trace bundle layout",
        ),
        ("trace_id", json!(""), "identity must not be empty"),
        ("root_thread_id", json!(""), "identity must not be empty"),
        ("rollout_id", json!(""), "identity must not be empty"),
    ] {
        let (temp, _) = protocol_bundle()?;
        let path = temp.path().join(MANIFEST_FILE_NAME);
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path)?)?;
        manifest[field] = replacement;
        fs::write(path, serde_json::to_vec(&manifest)?)?;
        expect_failure(&temp, expected);
    }
    Ok(())
}

#[test]
fn event_schema_sequence_and_rollout_identity_are_admitted() -> Result<()> {
    for (field, replacement, expected) in [
        ("schema_version", json!(2), "unsupported trace event schema"),
        ("seq", json!(0), "sequence must be contiguous"),
        ("seq", json!(1), "sequence must be contiguous"),
        ("seq", json!(3), "sequence must be contiguous"),
        (
            "rollout_id",
            json!("another-rollout"),
            "rollout identity disagrees",
        ),
    ] {
        let (temp, _) = protocol_bundle()?;
        let mut events = read_events(&temp)?;
        events[1][field] = replacement;
        write_events(&temp, &events)?;
        expect_failure(&temp, expected);
    }
    Ok(())
}

#[test]
fn present_rollout_start_must_match_manifest_and_position() -> Result<()> {
    for (index, trace_id, root_id, expected) in [
        (
            0,
            "different",
            "thread-root",
            "identity disagrees with manifest",
        ),
        (
            0,
            "trace-1",
            "different",
            "identity disagrees with manifest",
        ),
        (1, "trace-1", "thread-root", "must be the first trace event"),
    ] {
        let (temp, _) = protocol_bundle()?;
        let mut events = read_events(&temp)?;
        events[index]["payload"] =
            json!({"type":"rollout_started", "trace_id":trace_id, "root_thread_id":root_id});
        write_events(&temp, &events)?;
        expect_failure(&temp, expected);
    }
    Ok(())
}

#[test]
fn malformed_record_diagnostics_withhold_untrusted_text() -> Result<()> {
    let (temp, _) = protocol_bundle()?;
    let mut events = read_events(&temp)?;
    events[1]["payload"]["type"] = json!("SYNTHETIC_PRIVATE_CANARY");
    write_events(&temp, &events)?;
    let error = match admit_bundle(temp.path()) {
        Ok(_) => panic!("expected unknown event variant to fail"),
        Err(error) => format!("{error:#}"),
    };
    assert_eq!(error, "invalid trace event JSON at ordinal 2");
    Ok(())
}

#[test]
fn semantic_failure_diagnostics_withhold_private_values() -> Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;
    let request = writer.write_json_payload(
        RawPayloadKind::InferenceRequest,
        &json!({"input":[{"type":"SYNTHETIC_PRIVATE_CANARY"}]}),
    )?;
    append_inference_start(&writer, "attempt-1", "turn-1", request)?;
    let error = match admit_bundle(temp.path()) {
        Ok(_) => panic!("expected unsupported private item type to fail"),
        Err(error) => format!("{error:#}"),
    };
    assert_eq!(error, "trace semantic reduction failed at event ordinal 3");
    Ok(())
}

#[test]
fn record_boundaries_require_complete_nonempty_jsonl_lines() -> Result<()> {
    for (suffix, expected) in [
        (b"\n".as_slice(), "blank record"),
        (b" \t\r\n".as_slice(), "blank record"),
        (b"{}".as_slice(), "incomplete final record"),
    ] {
        let (temp, _) = protocol_bundle()?;
        let path = temp.path().join(RAW_EVENT_LOG_FILE_NAME);
        let mut bytes = fs::read(&path)?;
        bytes.extend_from_slice(suffix);
        fs::write(path, bytes)?;
        expect_failure(&temp, expected);
    }
    let (temp, _) = protocol_bundle()?;
    let path = temp.path().join(RAW_EVENT_LOG_FILE_NAME);
    let mut bytes = fs::read(&path)?;
    bytes.pop();
    fs::write(path, bytes)?;
    expect_failure(&temp, "incomplete final record");
    Ok(())
}

#[test]
fn every_protocol_only_payload_must_exist_and_parse() -> Result<()> {
    let (temp, reference) = protocol_bundle()?;
    fs::write(temp.path().join(&reference.path), b"{ incomplete")?;
    expect_failure(&temp, "invalid JSON in raw_payload:1");
    fs::remove_file(temp.path().join(reference.path))?;
    expect_failure(&temp, "read raw_payload:1");
    Ok(())
}

#[test]
fn ignored_payload_strings_and_keys_must_be_utf8() -> Result<()> {
    for invalid in [
        b"\xff".as_slice(),
        b"\xc0\xaf".as_slice(),
        b"\xe2\x82".as_slice(),
        b"\xed\xa0\x80".as_slice(),
    ] {
        for (prefix, suffix) in [
            (b"\"SYNTHETIC_PRIVATE_CANARY".as_slice(), b"\"".as_slice()),
            (
                b"{\"SYNTHETIC_PRIVATE_CANARY".as_slice(),
                b"\":0}".as_slice(),
            ),
            (
                b"{\"ignored\":[{\"value\":\"SYNTHETIC_PRIVATE_CANARY".as_slice(),
                b"\"}]}".as_slice(),
            ),
        ] {
            let (temp, reference) = protocol_bundle()?;
            let bytes = [prefix, invalid, suffix].concat();
            fs::write(temp.path().join(&reference.path), bytes)?;
            let error = match admit_bundle(temp.path()) {
                Ok(_) => panic!("expected non-UTF-8 payload to fail admission"),
                Err(error) => format!("{error:#}"),
            };
            assert_eq!(error, "invalid UTF-8 in raw_payload:1");
        }
    }
    Ok(())
}

#[test]
fn ignored_manifest_and_event_fields_must_be_utf8() -> Result<()> {
    for (name, expected) in [
        (MANIFEST_FILE_NAME, "trace manifest must be UTF-8"),
        (RAW_EVENT_LOG_FILE_NAME, "trace event log must be UTF-8"),
    ] {
        let (temp, _) = protocol_bundle()?;
        let path = temp.path().join(name);
        let mut bytes = fs::read(&path)?;
        let closing_brace = bytes.iter().rposition(|byte| *byte == b'}').unwrap();
        bytes.truncate(closing_brace);
        bytes.extend_from_slice(b",\"unused\":\"SYNTHETIC_PRIVATE_CANARY\xff\"}\n");
        fs::write(path, bytes)?;
        let error = match admit_bundle(temp.path()) {
            Ok(_) => panic!("expected ignored non-UTF-8 field to fail admission"),
            Err(error) => format!("{error:#}"),
        };
        assert_eq!(error, expected);
    }
    Ok(())
}

#[test]
fn admitted_unicode_payload_bytes_are_preserved_exactly() -> Result<()> {
    let (temp, reference) = protocol_bundle()?;
    let bytes = " {\"ignored\":{\"雪\":[\"é\",\"🙂\",\"\\u96ea\"]}} \n".as_bytes();
    fs::write(temp.path().join(&reference.path), bytes)?;
    let admitted = admit_bundle(temp.path())?;
    assert_eq!(admitted.payloads[&reference.raw_payload_id], bytes);
    Ok(())
}

#[test]
fn reference_identity_path_and_kind_cannot_conflict() -> Result<()> {
    for (field, replacement, expected) in [
        (
            "path",
            json!("../outside.json"),
            "identity and path disagree",
        ),
        ("path", json!("/outside.json"), "identity and path disagree"),
        (
            "path",
            json!("payloads/2.json"),
            "identity and path disagree",
        ),
        (
            "raw_payload_id",
            json!("raw_payload:01"),
            "identity and path disagree",
        ),
        (
            "raw_payload_id",
            json!("raw_payload:0"),
            "invalid raw payload identity",
        ),
        (
            "raw_payload_id",
            json!(format!("raw_payload:{}", MAX_PAYLOAD_COUNT + 1)),
            "invalid raw payload identity",
        ),
        (
            "kind",
            json!({"type":"tool_result"}),
            "reused with a different reference",
        ),
    ] {
        let (temp, _) = protocol_bundle()?;
        let mut events = read_events(&temp)?;
        events[2]["payload"]["event_payload"][field] = replacement;
        write_events(&temp, &events)?;
        expect_failure(&temp, expected);
    }
    Ok(())
}

#[test]
fn byte_limits_fail_before_reading_oversized_sparse_files() -> Result<()> {
    for (name, length) in [
        (MANIFEST_FILE_NAME, MAX_RECORD_BYTES + 1),
        (RAW_EVENT_LOG_FILE_NAME, MAX_BUNDLE_BYTES + 1),
        ("payloads/1.json", MAX_RECORD_BYTES + 1),
    ] {
        let (temp, _) = protocol_bundle()?;
        OpenOptions::new()
            .write(true)
            .open(temp.path().join(name))?
            .set_len(length as u64)?;
        expect_failure(&temp, "exceeds its byte limit");
    }
    Ok(())
}

#[test]
fn byte_budget_counts_actual_bytes_without_truncation() -> Result<()> {
    let (temp, reference) = protocol_bundle()?;
    let path = temp.path().join(reference.path);
    let directory = temp.path().join("payloads");
    fs::write(&path, b"null")?;
    let mut remaining = 4;
    assert_eq!(
        read_file(&path, &directory, /*limit*/ 4, &mut remaining)?,
        b"null"
    );
    assert_eq!(remaining, 0);
    assert!(read_file(&path, &directory, /*limit*/ 4, &mut remaining).is_err());
    let mut remaining = 8;
    assert!(read_file(&path, &directory, /*limit*/ 3, &mut remaining).is_err());
    assert_eq!(remaining, 8);
    Ok(())
}

#[test]
fn event_byte_limit_includes_the_line_terminator() -> Result<()> {
    let (temp, _) = protocol_bundle()?;
    let mut bytes = vec![b' '; MAX_RECORD_BYTES];
    bytes.push(b'\n');
    fs::write(temp.path().join(RAW_EVENT_LOG_FILE_NAME), &bytes)?;
    expect_failure(&temp, "record byte limit including newline");
    bytes.remove(0);
    fs::write(temp.path().join(RAW_EVENT_LOG_FILE_NAME), &bytes)?;
    expect_failure(&temp, "blank");
    Ok(())
}

#[test]
fn event_count_is_bounded_even_when_payload_is_reused() -> Result<()> {
    let (temp, _) = protocol_bundle()?;
    let mut events = read_events(&temp)?;
    let template = events[1].clone();
    while (events.len() as u64) <= MAX_EVENT_COUNT {
        let mut event = template.clone();
        event["seq"] = json!(events.len() + 1);
        events.push(event);
    }
    write_events(&temp, &events)?;
    expect_failure(&temp, "event count limit");
    Ok(())
}

#[test]
fn inferred_turn_closure_preserves_absent_provider_terminal_evidence() -> Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;
    let request =
        writer.write_json_payload(RawPayloadKind::InferenceRequest, &json!({"input":[]}))?;
    append_inference_start(&writer, "attempt-1", "turn-1", request)?;
    writer.append(RawTraceEventPayload::CodexTurnEnded {
        codex_turn_id: "turn-1".to_string(),
        status: ExecutionStatus::Completed,
    })?;
    let admitted = admit_bundle(temp.path())?;
    assert_eq!(
        admitted.rollout.inference_calls["attempt-1"]
            .execution
            .status,
        ExecutionStatus::Cancelled
    );
    assert!(admitted.events.iter().all(|event| !matches!(
        event.payload,
        RawTraceEventPayload::InferenceCompleted { .. }
            | RawTraceEventPayload::InferenceFailed { .. }
            | RawTraceEventPayload::InferenceCancelled { .. }
    )));
    Ok(())
}

#[test]
fn inference_terminal_identity_and_context_are_not_overwritten() -> Result<()> {
    for (field, replacement, expected) in [
        (
            "thread_id",
            json!("another-thread"),
            "envelope thread disagrees",
        ),
        (
            "codex_turn_id",
            json!("another-turn"),
            "envelope turn disagrees",
        ),
        (
            "thread_id",
            Value::Null,
            "duplicate inference terminal event",
        ),
    ] {
        let temp = private_tempdir()?;
        let writer = create_started_writer(&temp)?;
        start_turn(&writer, "turn-1")?;
        let request =
            writer.write_json_payload(RawPayloadKind::InferenceRequest, &json!({"input":[]}))?;
        append_inference_start(&writer, "attempt-1", "turn-1", request)?;
        let terminal = RawTraceEventPayload::InferenceCancelled {
            inference_call_id: "attempt-1".to_string(),
            upstream_request_id: None,
            reason: "synthetic".to_string(),
            partial_response_payload: None,
        };
        writer.append(terminal.clone())?;
        writer.append(terminal)?;
        let mut events = read_events(&temp)?;
        events[4][field] = replacement;
        write_events(&temp, &events)?;
        expect_failure(&temp, expected);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn private_modes_and_single_link_files_are_required() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for name in [
        MANIFEST_FILE_NAME,
        RAW_EVENT_LOG_FILE_NAME,
        "payloads/1.json",
        "payloads",
        "",
    ] {
        let (temp, _) = protocol_bundle()?;
        fs::set_permissions(temp.path().join(name), fs::Permissions::from_mode(0o755))?;
        expect_failure(&temp, "must be private");
    }
    let (temp, reference) = protocol_bundle()?;
    fs::hard_link(
        temp.path().join(reference.path),
        temp.path().join("second-link.json"),
    )?;
    expect_failure(&temp, "must not have hard links");
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinked_bundle_components_are_rejected() -> Result<()> {
    use std::os::unix::fs::symlink;
    for name in [
        MANIFEST_FILE_NAME,
        RAW_EVENT_LOG_FILE_NAME,
        "payloads/1.json",
        "payloads",
    ] {
        let (temp, _) = protocol_bundle()?;
        let path = temp.path().join(name);
        let target = temp.path().join("original");
        fs::rename(&path, &target)?;
        symlink(&target, path)?;
        expect_failure(&temp, "must not be linked or special");
    }
    let (temp, _) = protocol_bundle()?;
    let wrapper = private_tempdir()?;
    let link = wrapper.path().join("linked-bundle");
    symlink(temp.path(), &link)?;
    assert!(admit_bundle(link).is_err());
    Ok(())
}
