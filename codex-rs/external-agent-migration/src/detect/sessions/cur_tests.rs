use super::*;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use std::fs::FileTimes;
use std::fs::OpenOptions;
use std::time::Duration;
use std::time::SystemTime;
use tempfile::TempDir;

#[test]
fn detects_cur_transcript_with_project_cwd() {
    let root = TempDir::new().expect("tempdir");
    let project_root = root.path().join("workspace with.dots_and-dashes");
    fs::create_dir_all(&project_root).expect("project root");
    let external_agent_home = root.path().join(".external");
    let encoded_project = encode_project_path(&project_root);
    let transcript = write_transcript(
        &external_agent_home,
        &encoded_project,
        "a-session",
        "first request",
    );

    let sessions =
        detect_recent_cur_sessions(&external_agent_home, root.path()).expect("detect sessions");

    assert_eq!(
        sessions,
        vec![ExternalAgentSessionMigration {
            path: transcript,
            cwd: project_root,
            title: Some("first request".to_string()),
        }]
    );
}

#[test]
fn detects_projectless_cur_transcript_without_embedded_metadata() {
    let root = TempDir::new().expect("tempdir");
    let external_agent_home = root.path().join(".cursor");
    let transcript = write_transcript(
        &external_agent_home,
        "empty-window",
        "projectless-session",
        "first request",
    );

    let sessions =
        detect_recent_cur_sessions(&external_agent_home, root.path()).expect("detect sessions");

    assert_eq!(
        sessions,
        vec![ExternalAgentSessionMigration {
            path: transcript,
            cwd: root.path().to_path_buf(),
            title: Some("first request".to_string()),
        }]
    );
}

#[test]
fn resolves_projectless_cur_cwd_from_relative_home() {
    let current_dir = std::env::current_dir().expect("current dir");

    assert_eq!(
        cur_project_cwd(
            Path::new(".cursor/projects/empty-window"),
            Path::new(".cursor"),
        ),
        Some(current_dir)
    );
}

#[test]
fn detects_cur_transcript_with_embedded_unc_cwd() {
    let root = TempDir::new().expect("tempdir");
    let external_agent_home = root.path().join(".external");
    let encoded_project = "server-share-repo";
    let unc_cwd = PathBuf::from(r"\\server\share\repo");
    let transcript = external_agent_home
        .join("projects")
        .join(encoded_project)
        .join("agent-transcripts")
        .join("unc-session/unc-session.jsonl");
    fs::create_dir_all(transcript.parent().expect("transcript parent"))
        .expect("transcript directory");
    fs::write(
        &transcript,
        [
            serde_json::json!({
                "cwd": unc_cwd,
                "role": "user",
                "timestamp_ms": 1_800_000_000_000_i64,
                "message": {
                    "content": [{
                        "type": "text",
                        "text": "<user_query>first request</user_query>",
                    }],
                },
            })
            .to_string(),
            serde_json::json!({
                "role": "assistant",
                "message": {
                    "content": [{"type": "text", "text": "first answer"}],
                },
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("transcript");

    assert_eq!(
        detect_recent_cur_sessions(&external_agent_home, root.path()).expect("detect sessions"),
        vec![ExternalAgentSessionMigration {
            path: transcript,
            cwd: unc_cwd,
            title: Some("first request".to_string()),
        }]
    );
}

#[test]
fn skips_cur_subagent_transcripts() {
    let root = TempDir::new().expect("tempdir");
    let project_root = root.path().join("workspace");
    fs::create_dir_all(&project_root).expect("project root");
    let external_agent_home = root.path().join(".external");
    let encoded_project = encode_project_path(&project_root);
    let transcript = write_transcript(
        &external_agent_home,
        &encoded_project,
        "main-session",
        "first request",
    );
    let subagent_transcript = external_agent_home
        .join("projects")
        .join(&encoded_project)
        .join("agent-transcripts")
        .join("main-session/subagents/worker/worker.jsonl");
    fs::create_dir_all(
        subagent_transcript
            .parent()
            .expect("subagent transcript parent"),
    )
    .expect("subagent transcript directory");
    fs::write(&subagent_transcript, transcript_contents("first request"))
        .expect("subagent transcript");

    let sessions =
        detect_recent_cur_sessions(&external_agent_home, root.path()).expect("detect sessions");

    assert_eq!(
        sessions,
        vec![ExternalAgentSessionMigration {
            path: transcript,
            cwd: project_root,
            title: Some("first request".to_string()),
        }]
    );
}

#[test]
fn rejects_ambiguous_encoded_project_cwd() {
    let root = TempDir::new().expect("tempdir");
    let nested_project = root.path().join("workspace").join("nested");
    let hyphenated_project = root.path().join("workspace-nested");
    fs::create_dir_all(&nested_project).expect("nested project");
    fs::create_dir_all(&hyphenated_project).expect("hyphenated project");

    assert_eq!(
        decode_cur_project_path(&encode_project_path(&nested_project)),
        None
    );
}

#[test]
fn resolves_cur_project_names_with_common_separators() {
    for (project_name, encoded_name) in [
        ("project", "project"),
        ("my-project", "my-project"),
        ("my--project", "my-project"),
        ("my project", "my-project"),
        ("my.project", "my-project"),
        ("my..project", "my-project"),
        ("my_project", "my-project"),
        ("my+project", "my-project"),
        ("my@project", "my-project"),
        ("my&project", "my-project"),
        ("my-awesome-project", "my-awesome-project"),
        ("my-awesome-cool-project", "my-awesome-cool-project"),
    ] {
        let root = TempDir::new().expect("tempdir");
        let project = root.path().join(project_name);
        fs::create_dir_all(&project).expect("project root");
        let encoded = format!("{}-{encoded_name}", encode_project_path(root.path()));

        assert_eq!(decode_cur_project_path(&encoded), Some(project));
    }
}

#[test]
fn rejects_ambiguous_cur_project_without_a_direct_match() {
    let root = TempDir::new().expect("tempdir");
    for project_name in ["my-project", "my project", "my+project"] {
        fs::create_dir_all(root.path().join(project_name)).expect("project root");
    }
    let encoded = format!("{}-my-project", encode_project_path(root.path()));

    assert_eq!(decode_cur_project_path(&encoded), None);
}

#[test]
fn rejects_ambiguous_cur_project_with_punctuated_ancestor() {
    let root = TempDir::new().expect("tempdir");
    let punctuated_ancestor = root.path().join("a-b").join("c");
    let punctuated_leaf = root.path().join("a").join("b-c");
    fs::create_dir_all(&punctuated_ancestor).expect("punctuated ancestor project");
    fs::create_dir_all(&punctuated_leaf).expect("punctuated leaf project");
    let encoded = encode_project_path(&punctuated_ancestor);

    assert_eq!(encoded, encode_project_path(&punctuated_leaf));
    assert_eq!(decode_cur_project_path(&encoded), None);
}

#[test]
fn rejects_ambiguous_cur_project_with_multiple_punctuated_ancestors() {
    for (first, second) in [
        (&["a-b", "c-d", "e"][..], &["a", "b", "c", "d-e"][..]),
        (&["a-b", "c-d"][..], &["a", "b", "c", "d"][..]),
    ] {
        let root = TempDir::new().expect("tempdir");
        let first = first
            .iter()
            .fold(root.path().to_path_buf(), |path, component| {
                path.join(component)
            });
        let second = second
            .iter()
            .fold(root.path().to_path_buf(), |path, component| {
                path.join(component)
            });
        fs::create_dir_all(&first).expect("first project");
        fs::create_dir_all(&second).expect("second project");
        let encoded = encode_project_path(&first);

        assert_eq!(encoded, encode_project_path(&second));
        assert_eq!(decode_cur_project_path(&encoded), None);
    }
}

#[test]
fn parses_windows_cursor_fixture_project_directory() {
    assert_eq!(
        decode_cur_windows_project_drive("C--Users-fixture-Cursor"),
        Some(('C', "-Users-fixture-Cursor"))
    );
    assert_eq!(
        decode_cur_windows_project_drive("C-Users-fixture-Cursor"),
        Some(('C', "Users-fixture-Cursor"))
    );
    assert_eq!(decode_cur_windows_project_drive("1-Users-fixture"), None);
}

#[test]
fn ignores_cur_sessions_older_than_import_window() {
    let root = TempDir::new().expect("tempdir");
    let project_root = root.path().join("workspace");
    fs::create_dir_all(&project_root).expect("project root");
    let external_agent_home = root.path().join(".external");
    let transcript = write_transcript(
        &external_agent_home,
        &encode_project_path(&project_root),
        "old-session",
        "old request",
    );
    set_modified_at(
        &transcript,
        SystemTime::UNIX_EPOCH + Duration::from_secs(/*secs*/ 1),
    );

    assert!(
        detect_recent_cur_sessions(&external_agent_home, root.path())
            .expect("detect sessions")
            .is_empty()
    );
}

#[test]
fn detects_cur_sessions_in_batches_and_redetects_modified_imports() {
    let root = TempDir::new().expect("tempdir");
    let project_root = root.path().join("workspace");
    fs::create_dir_all(&project_root).expect("project root");
    let external_agent_home = root.path().join(".external");
    let encoded_project = encode_project_path(&project_root);
    let modified_at = SystemTime::now();
    let mut expected = Vec::new();
    let default_limits = ExternalAgentSessionImportLimits::default();
    for index in 0..=default_limits.max_sessions {
        let session_id = format!("session-{index:02}");
        let title = format!("request {index}");
        let path = write_transcript(&external_agent_home, &encoded_project, &session_id, &title);
        set_modified_at(
            &path,
            modified_at - Duration::from_secs(/*secs*/ index as u64),
        );
        expected.push(ExternalAgentSessionMigration {
            path,
            cwd: project_root.clone(),
            title: Some(title),
        });
    }
    let oldest_session = expected.pop().expect("oldest session");

    let sessions =
        detect_recent_cur_sessions(&external_agent_home, root.path()).expect("detect sessions");

    assert_eq!(sessions, expected);
    for session in &sessions {
        crate::sessions::ledger::record_imported_session(
            root.path(),
            &session.path,
            ThreadId::new(),
        )
        .expect("record import");
    }

    assert_eq!(
        detect_recent_cur_sessions(&external_agent_home, root.path()).expect("detect sessions"),
        vec![oldest_session.clone()]
    );
    crate::sessions::ledger::record_imported_session(
        root.path(),
        &oldest_session.path,
        ThreadId::new(),
    )
    .expect("record oldest import");
    assert!(
        detect_recent_cur_sessions(&external_agent_home, root.path())
            .expect("detect sessions")
            .is_empty()
    );

    let modified_session = &expected[0];
    let updated_record = serde_json::json!({
        "role": "assistant",
        "message": {
            "content": [{"type": "text", "text": "updated answer"}],
        },
    })
    .to_string();
    fs::write(
        &modified_session.path,
        format!(
            "{}\n{updated_record}",
            transcript_contents(modified_session.title.as_deref().expect("session title"))
        ),
    )
    .expect("update transcript");
    set_modified_at(
        &modified_session.path,
        SystemTime::now() + Duration::from_secs(/*secs*/ 1),
    );

    assert_eq!(
        detect_recent_cur_sessions(&external_agent_home, root.path()).expect("detect sessions"),
        vec![modified_session.clone()]
    );
}

#[cfg(not(windows))]
#[test]
fn detects_cur_transcript_with_multiple_punctuated_ancestors() {
    let root = TempDir::new().expect("tempdir");
    let project_root = root.path().join("outer-one/middle-two/my-project");
    fs::create_dir_all(&project_root).expect("nested project");
    let external_agent_home = root.path().join(".external");
    let transcript = write_transcript(
        &external_agent_home,
        &encode_project_path(&project_root),
        "nested-session",
        "nested request",
    );

    assert_eq!(
        detect_recent_cur_sessions(&external_agent_home, root.path()).expect("detect sessions"),
        vec![ExternalAgentSessionMigration {
            path: transcript,
            cwd: project_root,
            title: Some("nested request".to_string()),
        }]
    );
}

#[cfg(not(windows))]
#[test]
fn rejects_distinct_complete_paths_across_multiple_partitions() {
    let root = TempDir::new().expect("tempdir");
    fs::create_dir_all(root.path().join("a-b/c-d")).expect("first project");
    fs::create_dir_all(root.path().join("a/b-c/d")).expect("second project");

    assert_eq!(
        resolve_cur_project_components(
            root.path(),
            &["a", "b", "c", "d"],
            CUR_PROJECT_PATH_PROBES_PER_COMPONENT * 4,
        ),
        None
    );
}

#[cfg(not(windows))]
#[test]
fn rejects_probe_exhaustion_even_after_finding_a_complete_path() {
    let root = TempDir::new().expect("tempdir");
    let project = root.path().join("a-b");
    fs::create_dir(&project).expect("project");
    fs::create_dir_all(root.path().join("a/c")).expect("unmatched alternate partition");

    // The first prefix generates one literal plus all two-part separators.
    // After a-b matches, the alternate a/b still requires one native probe.
    let root_probes = 1 + CUR_PROJECT_SEPARATORS.len();
    for (probes, expected) in [(root_probes, None), (root_probes + 1, Some(project))] {
        assert_eq!(
            resolve_cur_project_components(root.path(), &["a", "b"], probes),
            expected,
            "probes={probes}"
        );
    }
}

#[cfg(unix)]
#[test]
fn follows_directory_symlinks_without_replacing_the_lexical_project_path() {
    let root = TempDir::new().expect("tempdir");
    let target = root.path().join("target/child");
    fs::create_dir_all(&target).expect("target");
    std::os::unix::fs::symlink(root.path().join("target"), root.path().join("linked"))
        .expect("directory symlink");

    assert_eq!(
        resolve_cur_project_components(
            root.path(),
            &["linked", "child"],
            CUR_PROJECT_PATH_PROBES_PER_COMPONENT * 2,
        ),
        Some(root.path().join("linked/child"))
    );
}

#[cfg(not(windows))]
#[test]
fn confirms_ascii_case_matches_using_native_filesystem_lookup() {
    let root = TempDir::new().expect("tempdir");
    fs::create_dir(root.path().join("MiXeD-NaMe")).expect("mixed-case project");
    for components in [["MiXeD", "NaMe"], ["mixed", "name"]] {
        let candidate = root.path().join(components.join("-"));
        let expected = candidate.is_dir().then_some(candidate);
        assert_eq!(
            resolve_cur_project_components(
                root.path(),
                &components,
                components.len() * CUR_PROJECT_PATH_PROBES_PER_COMPONENT,
            ),
            expected
        );
    }
}

#[cfg(not(windows))]
#[test]
fn leaves_unicode_collation_and_normalization_to_native_lookup() {
    for (directory, components) in [
        ("e\u{301}cole-repo", ["école", "repo"]),
        ("K-repo", ["K", "repo"]),
    ] {
        let root = TempDir::new().expect("tempdir");
        fs::create_dir(root.path().join(directory)).expect("Unicode project");
        let candidate = root.path().join(components.join("-"));
        let expected = candidate.is_dir().then_some(candidate);
        assert_eq!(
            resolve_cur_project_components(
                root.path(),
                &components,
                components.len() * CUR_PROJECT_PATH_PROBES_PER_COMPONENT,
            ),
            expected
        );
    }
}

#[cfg(unix)]
#[test]
fn rejects_native_probe_errors_even_after_finding_a_complete_path() {
    let root = TempDir::new().expect("tempdir");
    fs::create_dir(root.path().join("a-b")).expect("complete candidate");
    fs::create_dir(root.path().join("a")).expect("alternate prefix");
    let loop_path = root.path().join("a/b");
    std::os::unix::fs::symlink("b", &loop_path).expect("owned symlink loop");
    let error = fs::metadata(&loop_path).expect_err("loop must fail native lookup");
    assert!(!matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ));

    // a-b is visited first, but the unresolved a/b alternative prevents uniqueness.
    assert_eq!(
        resolve_cur_project_components(
            root.path(),
            &["a", "b"],
            CUR_PROJECT_PATH_PROBES_PER_COMPONENT * 2,
        ),
        None
    );
}

#[cfg(unix)]
#[test]
fn ignores_absent_dangling_and_non_directory_candidates() {
    for kind in ["absent", "file", "dangling", "not-a-directory"] {
        let root = TempDir::new().expect("tempdir");
        let project = root.path().join("a-b");
        fs::create_dir(&project).expect("complete candidate");
        let alternate = root.path().join("a");
        match kind {
            "absent" => {}
            "file" => fs::write(&alternate, b"fixture").expect("non-directory candidate"),
            "dangling" => {
                std::os::unix::fs::symlink("missing", &alternate).expect("dangling candidate");
                assert_eq!(
                    fs::metadata(&alternate)
                        .expect_err("dangling lookup")
                        .kind(),
                    io::ErrorKind::NotFound,
                );
            }
            "not-a-directory" => {
                fs::write(root.path().join("file"), b"fixture").expect("non-directory parent");
                std::os::unix::fs::symlink("file/child", &alternate)
                    .expect("non-directory candidate target");
                assert_eq!(
                    fs::metadata(&alternate)
                        .expect_err("invalid parent lookup")
                        .kind(),
                    io::ErrorKind::NotADirectory,
                );
            }
            _ => unreachable!("fixed fixture kinds"),
        }
        assert_eq!(
            resolve_cur_project_components(
                root.path(),
                &["a", "b"],
                CUR_PROJECT_PATH_PROBES_PER_COMPONENT * 2,
            ),
            Some(project),
            "{kind} alternative must not hide the complete candidate"
        );
    }
}

#[cfg(not(windows))]
#[test]
fn unrelated_entries_do_not_consume_project_candidate_budget() {
    let root = TempDir::new().expect("tempdir");
    let project = root.path().join("a-b");
    fs::create_dir(&project).expect("project");
    let probes = 1 + CUR_PROJECT_SEPARATORS.len();
    assert_eq!(
        resolve_cur_project_components(root.path(), &["a", "b"], probes),
        Some(project.clone())
    );
    for index in 0..4097 {
        fs::write(root.path().join(format!("unrelated-{index}")), b"fixture")
            .expect("unrelated directory entry");
    }
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        fs::write(root.path().join(OsString::from_vec(vec![0xff])), b"fixture")
            .expect("unrelated non-UTF8 entry");
    }
    assert_eq!(
        resolve_cur_project_components(root.path(), &["a", "b"], probes),
        Some(project)
    );
}

#[cfg(not(windows))]
#[test]
fn input_proportional_budget_resolves_many_literal_ancestors() {
    let root = TempDir::new().expect("tempdir");
    let components = ["one", "two", "three", "four", "five", "six"];
    let project = root.path().join(components.join("/"));
    fs::create_dir_all(&project).expect("deep project");
    assert_eq!(
        resolve_cur_project_components(
            root.path(),
            &components,
            components.len() * CUR_PROJECT_PATH_PROBES_PER_COMPONENT,
        ),
        Some(project)
    );
}

fn write_transcript(
    external_agent_home: &Path,
    encoded_project: &str,
    session_id: &str,
    first_request: &str,
) -> PathBuf {
    let transcript = external_agent_home
        .join("projects")
        .join(encoded_project)
        .join("agent-transcripts")
        .join(session_id)
        .join(format!("{session_id}.jsonl"));
    fs::create_dir_all(transcript.parent().expect("transcript parent"))
        .expect("transcript directory");
    fs::write(&transcript, transcript_contents(first_request)).expect("transcript");
    transcript
}

fn transcript_contents(first_request: &str) -> String {
    [
        serde_json::json!({
            "role": "user",
            "message": {
                "content": [{
                    "type": "text",
                    "text": format!("<user_query>{first_request}</user_query>"),
                }],
            },
        })
        .to_string(),
        serde_json::json!({
            "role": "assistant",
            "message": {
                "content": [{"type": "text", "text": "first answer"}],
            },
        })
        .to_string(),
    ]
    .join("\n")
}

fn set_modified_at(path: &Path, modified_at: SystemTime) {
    OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open transcript")
        .set_times(FileTimes::new().set_modified(modified_at))
        .expect("set transcript modified time");
}

#[cfg(windows)]
fn encode_project_path(path: &Path) -> String {
    path.to_string_lossy().replace([':', '\\', '/'], "-")
}

#[cfg(not(windows))]
fn encode_project_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-")
}
