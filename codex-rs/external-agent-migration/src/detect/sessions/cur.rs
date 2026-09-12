//! Cursor project names encode both path boundaries and punctuation as dashes.
//! Non-Windows decoding probes generated path partitions under an input-proportional
//! work budget, rejecting ambiguity or incomplete search. Unrelated directory entries
//! do not affect admission; native lookup owns case, normalization, and symlink behavior.
//! Windows retains its existing decoder pending native platform qualification.

use super::common::SessionFileCandidate;
use super::common::detect_recent_sessions;
use crate::model::ExternalAgentSessionImportLimits;
use crate::sessions::ExternalAgentSessionMigration;
use crate::sessions::SessionRecordFormat;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

#[cfg(windows)]
const MAX_CUR_PROJECT_PATH_PROBES: usize = 128;
const CUR_PROJECT_SEPARATORS: [&str; 11] =
    ["-", "_", ".", " ", "--", "..", "__", "  ", "+", "@", "&"];
// Each prefix has one literal candidate and at most three merge lengths.
// Budget one complete prefix expansion per encoded component. Extra ambiguous
// branches share that finite budget and exhaustion never establishes uniqueness.
#[cfg(not(windows))]
const CUR_PROJECT_PATH_PROBES_PER_COMPONENT: usize = 1 + 3 * CUR_PROJECT_SEPARATORS.len();

pub fn detect_recent_cur_sessions(
    external_agent_home: &Path,
    codex_home: &Path,
) -> io::Result<Vec<ExternalAgentSessionMigration>> {
    detect_recent_cur_sessions_with_limits(
        external_agent_home,
        codex_home,
        ExternalAgentSessionImportLimits::default(),
    )
}

pub(crate) fn detect_recent_cur_sessions_with_limits(
    external_agent_home: &Path,
    codex_home: &Path,
    limits: ExternalAgentSessionImportLimits,
) -> io::Result<Vec<ExternalAgentSessionMigration>> {
    let projects_root = external_agent_home.join("projects");
    if !projects_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    for project_entry in fs::read_dir(projects_root)? {
        let Ok(project_entry) = project_entry else {
            continue;
        };
        let project_storage = project_entry.path();
        if !project_storage.is_dir() {
            continue;
        }
        let fallback_cwd = cur_project_cwd(&project_storage, external_agent_home);
        for path in cur_transcript_files(&project_storage.join("agent-transcripts")) {
            candidates.push(SessionFileCandidate {
                path,
                fallback_cwd: fallback_cwd.clone(),
                record_format: SessionRecordFormat::Cur,
            });
        }
    }
    detect_recent_sessions(
        codex_home, candidates, /*require_existing_cwd*/ false, limits,
    )
}

fn cur_transcript_files(transcripts_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![transcripts_root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if entry.file_name() != "subagents" {
                    pending.push(path);
                }
            } else if file_type.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn cur_project_cwd(project_storage: &Path, external_agent_home: &Path) -> Option<PathBuf> {
    let encoded = project_storage.file_name()?.to_str()?;
    // Cursor stores projectless chats under this reserved project name.
    if encoded == "empty-window" {
        let external_agent_home = if external_agent_home.is_absolute() {
            external_agent_home.to_path_buf()
        } else {
            std::env::current_dir().ok()?.join(external_agent_home)
        };
        return external_agent_home.parent().map(Path::to_path_buf);
    }
    decode_cur_project_path(encoded)
}

#[cfg(not(windows))]
fn decode_cur_project_path(encoded: &str) -> Option<PathBuf> {
    let encoded = encoded.strip_prefix('-').unwrap_or(encoded);
    let components: Vec<_> = encoded.split('-').collect();
    if components.iter().any(|component| {
        component.is_empty()
            || matches!(*component, "." | "..")
            || component.contains(['/', '\\', ':'])
    }) {
        return None;
    }
    let max_probes = components
        .len()
        .checked_mul(CUR_PROJECT_PATH_PROBES_PER_COMPONENT)?;
    resolve_cur_project_components(Path::new("/"), &components, max_probes)
}

/// Resolves one complete, unambiguous lexical path without canonicalizing away
/// symlink aliases. Every resolved prefix shares the same native probe budget.
#[cfg(not(windows))]
fn resolve_cur_project_components(
    root: &Path,
    components: &[&str],
    max_probes: usize,
) -> Option<PathBuf> {
    let mut pending = vec![(root.to_path_buf(), 0)];
    let mut matched_path = None;
    let mut probes = 0;
    while let Some((parent, consumed)) = pending.pop() {
        if consumed == components.len() {
            if matched_path
                .as_ref()
                .is_some_and(|matched| matched != &parent)
            {
                return None;
            }
            matched_path = Some(parent);
            continue;
        }
        for length in 1..=4.min(components.len() - consumed) {
            let separators = if length == 1 {
                &[""][..]
            } else {
                &CUR_PROJECT_SEPARATORS[..]
            };
            for separator in separators {
                if probes >= max_probes {
                    return None;
                }
                probes += 1;
                let name = components[consumed..consumed + length].join(separator);
                let candidate = parent.join(name);
                match fs::metadata(&candidate) {
                    Ok(metadata) if metadata.is_dir() => {
                        pending.push((candidate, consumed + length));
                    }
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) => {}
                    Err(_) => return None,
                }
            }
        }
    }
    matched_path
}

// Keep the existing Windows decoder until the generated-probe traversal has
// actual Windows qualification for drive paths and native filename aliases.
#[cfg(windows)]
fn decode_cur_project_path(encoded: &str) -> Option<PathBuf> {
    #[cfg(not(windows))]
    let mut path = PathBuf::from("/");

    #[cfg(windows)]
    let (encoded, mut path) = {
        let (drive, encoded) = decode_cur_windows_project_drive(encoded)?;
        (encoded, PathBuf::from(format!("{drive}:\\")))
    };

    let encoded = encoded.strip_prefix('-').unwrap_or(encoded);
    for component in encoded.split('-') {
        if component.is_empty()
            || matches!(component, "." | "..")
            || component.contains(['/', '\\', ':'])
        {
            return None;
        }
        path.push(component);
    }

    let mut matched_path = None;
    let mut probes = 0;
    let mut inspect = |candidate: PathBuf| {
        if probes >= MAX_CUR_PROJECT_PATH_PROBES {
            return None;
        }
        probes += 1;
        if candidate.is_dir() {
            if matched_path
                .as_ref()
                .is_some_and(|matched_path| matched_path != &candidate)
            {
                return None;
            }
            matched_path = Some(candidate);
        }
        Some(())
    };
    inspect(path.clone())?;

    for suffix_length in 2..=4 {
        let mut parent = path.as_path();
        let mut suffix = Vec::with_capacity(suffix_length);
        for _ in 0..suffix_length {
            let Some(component) = parent.file_name().and_then(|name| name.to_str()) else {
                break;
            };
            suffix.push(component);
            let Some(ancestor) = parent.parent() else {
                break;
            };
            parent = ancestor;
        }
        if suffix.len() != suffix_length {
            break;
        }
        suffix.reverse();

        for separator in CUR_PROJECT_SEPARATORS {
            inspect(parent.join(suffix.join(separator)))?;
        }
    }

    let mut ancestor = path.parent();
    while let Some(right) = ancestor {
        let Some(right_name) = right.file_name().and_then(|name| name.to_str()) else {
            break;
        };
        let Some(left) = right.parent() else {
            break;
        };
        let Some(left_name) = left.file_name().and_then(|name| name.to_str()) else {
            break;
        };
        let Some(prefix) = left.parent() else {
            break;
        };
        let Ok(trailing) = path.strip_prefix(right) else {
            return None;
        };

        for separator in CUR_PROJECT_SEPARATORS {
            let merged_prefix = prefix.join(format!("{left_name}{separator}{right_name}"));
            if probes >= MAX_CUR_PROJECT_PATH_PROBES {
                return None;
            }
            probes += 1;
            if !merged_prefix.is_dir() {
                continue;
            }

            if probes >= MAX_CUR_PROJECT_PATH_PROBES {
                return None;
            }
            probes += 1;
            let candidate = merged_prefix.join(trailing);
            if !candidate.is_dir()
                || matched_path
                    .as_ref()
                    .is_some_and(|matched_path| matched_path != &candidate)
            {
                return None;
            }
            matched_path = Some(candidate);
        }
        ancestor = Some(left);
    }

    matched_path
}

#[cfg(any(windows, test))]
fn decode_cur_windows_project_drive(encoded: &str) -> Option<(char, &str)> {
    let drive = encoded.as_bytes().first().copied()?;
    if !drive.is_ascii_alphabetic() || encoded.as_bytes().get(1) != Some(&b'-') {
        return None;
    }

    Some((char::from(drive), encoded.get(2..)?))
}

#[cfg(test)]
#[path = "cur_tests.rs"]
mod tests;
