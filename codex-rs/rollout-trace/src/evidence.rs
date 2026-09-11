//! Finite, exact-byte projection of one admitted private trace snapshot.

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use serde::Serialize;
use serde_json::json;

use crate::admit_bundle;
use crate::limits::MAX_BUNDLE_BYTES;
use crate::reducer::AdmittedTraceBundle;
use crate::writer::JsonLayout;
use crate::writer::encode_record;

const DESCRIPTOR_BYTES: usize = 1024;

/// Write wire-format v1 evidence from one admitted in-memory snapshot.
///
/// Each bounded JSON descriptor ends with LF. Data descriptors are followed by
/// exactly their declared byte count, then a separate framing LF. Original
/// manifest, event-log and payload bytes are preserved; the existing reduced
/// graph is serialized separately for its links, not as provider-outcome proof.
/// The graph is bounded and encoded before any output is written.
///
/// This stream contains unredacted private data. The caller owns a private sink
/// and must redact before persistence/export. A receiver accepts the stream only
/// after matching the footer, EOF and successful writer completion. A failed
/// write can leave a prefix, never a complete successful stream. No source file
/// is reopened after admission and no output file is created by this function.
pub fn write_evidence(bundle_dir: &Path, output: &mut impl Write) -> Result<()> {
    let snapshot = admit_bundle(bundle_dir)?;
    write_snapshot(&snapshot, output, MAX_BUNDLE_BYTES)
}

fn write_snapshot(
    snapshot: &AdmittedTraceBundle,
    output: &mut impl Write,
    graph_limit: usize,
) -> Result<()> {
    // Reuse publication's serialization owner. A diagnostic graph can amplify
    // the source through repeated conversation content, so it has its own bound.
    let graph = encode_record(&snapshot.rollout, JsonLayout::CompactLine, graph_limit)?;
    let mut payloads = snapshot
        .payloads
        .iter()
        .map(|(id, bytes)| {
            let ordinal = id
                .strip_prefix("raw_payload:")
                .and_then(|ordinal| ordinal.parse::<u64>().ok())
                .context("admitted payload identity is unavailable")?;
            Ok((ordinal, bytes.as_slice()))
        })
        .collect::<Result<Vec<_>>>()?;
    // Canonical ordinals may have gaps. Lexical ID order would put 10 before 2.
    payloads.sort_unstable_by_key(|(ordinal, _)| *ordinal);
    let source_bytes = snapshot
        .manifest_json
        .len()
        .checked_add(snapshot.event_log_jsonl.len())
        .context("trace evidence source byte count overflow")?;
    let source_bytes = payloads
        .iter()
        .try_fold(source_bytes, |total, (_, bytes)| {
            total
                .checked_add(bytes.len())
                .context("trace evidence source byte count overflow")
        })?;
    ensure!(
        source_bytes <= MAX_BUNDLE_BYTES,
        "trace evidence source exceeds its byte limit"
    );
    let counts = EvidenceCounts {
        source_bytes,
        graph_bytes: graph.len(),
        event_count: snapshot.events.len(),
        payload_count: payloads.len(),
        data_records: payloads.len() + 3,
    };
    descriptor(
        output,
        &Header {
            kind: "header",
            format: "codex_trace_evidence",
            schema_version: 1,
            counts: &counts,
        },
    )?;
    frame(output, "manifest", &snapshot.manifest_json)?;
    frame(output, "events", &snapshot.event_log_jsonl)?;
    for (ordinal, bytes) in payloads {
        descriptor(
            output,
            &json!({"kind":"payload", "ordinal":ordinal, "bytes":bytes.len()}),
        )?;
        body(output, bytes)?;
    }
    frame(output, "graph", &graph)?;
    descriptor(
        output,
        &Footer {
            kind: "end",
            counts: &counts,
        },
    )?;
    output
        .flush()
        .map_err(|_| anyhow!("trace evidence flush failed"))
}

#[derive(Serialize)]
struct EvidenceCounts {
    source_bytes: usize,
    graph_bytes: usize,
    event_count: usize,
    payload_count: usize,
    data_records: usize,
}

#[derive(Serialize)]
struct Header<'a> {
    kind: &'static str,
    format: &'static str,
    schema_version: u32,
    #[serde(flatten)]
    counts: &'a EvidenceCounts,
}

#[derive(Serialize)]
struct Footer<'a> {
    kind: &'static str,
    #[serde(flatten)]
    counts: &'a EvidenceCounts,
}

fn descriptor(output: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = encode_record(value, JsonLayout::CompactLine, DESCRIPTOR_BYTES)?;
    output
        .write_all(&bytes)
        .map_err(|_| anyhow!("trace evidence descriptor write failed"))
}

fn frame(output: &mut impl Write, kind: &'static str, bytes: &[u8]) -> Result<()> {
    descriptor(output, &json!({"kind":kind, "bytes":bytes.len()}))?;
    body(output, bytes)
}

fn body(output: &mut impl Write, bytes: &[u8]) -> Result<()> {
    output
        .write_all(bytes)
        .and_then(|()| output.write_all(b"\n"))
        .map_err(|_| anyhow!("trace evidence body write failed"))
}

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
