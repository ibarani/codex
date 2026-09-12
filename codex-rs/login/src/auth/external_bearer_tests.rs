use std::num::NonZeroU64;

use codex_protocol::config_types::ModelProviderAuthInfo;
use pretty_assertions::assert_eq;

use super::run_provider_auth_command;

fn command_config(command: &str, args: &[&str]) -> ModelProviderAuthInfo {
    ModelProviderAuthInfo {
        command: command.to_string(),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        cwd: std::env::current_dir().unwrap().try_into().unwrap(),
        timeout_ms: NonZeroU64::new(5_000).unwrap(),
        refresh_interval_ms: 0,
    }
}

#[tokio::test]
async fn missing_command_does_not_expose_its_path() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("PRIVATE-CREDENTIAL-MARKER");
    let config = command_config(path.to_str().unwrap(), &[]);
    let error = run_provider_auth_command(&config).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(error.to_string(), "provider auth command failed to start");
}

#[tokio::test]
async fn failed_command_does_not_expose_either_output_stream() {
    #[cfg(unix)]
    let config = command_config(
        "/bin/sh",
        &[
            "-c",
            "printf PRIVATE-CREDENTIAL-MARKER; printf PRIVATE-CREDENTIAL-MARKER >&2; exit 7",
        ],
    );
    #[cfg(windows)]
    let config = command_config(
        "cmd.exe",
        &[
            "/d",
            "/s",
            "/c",
            "echo PRIVATE-CREDENTIAL-MARKER & echo PRIVATE-CREDENTIAL-MARKER 1>&2 & exit /b 7",
        ],
    );
    let error = run_provider_auth_command(&config).await.unwrap_err();
    let message = error.to_string();
    assert!(message.starts_with("provider auth command exited with status "));
    assert!(message.contains('7'));
    assert!(!format!("{error:?}").contains("PRIVATE-CREDENTIAL-MARKER"));
}

#[cfg(unix)]
#[tokio::test]
async fn command_output_requires_a_nonempty_valid_bearer_token() {
    for (script, expected) in [
        ("printf ''", "provider auth command produced an empty token"),
        (
            "printf '\\377'",
            "provider auth command wrote non-UTF-8 data to stdout",
        ),
        (
            "printf 'private\\ntoken'",
            "provider auth command produced an invalid bearer token",
        ),
    ] {
        let config = command_config("/bin/sh", &["-c", script]);
        let error = run_provider_auth_command(&config).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), expected);
    }
    let config = command_config("/bin/sh", &["-c", "printf '  valid-token\\n'"]);
    assert_eq!(
        run_provider_auth_command(&config).await.unwrap(),
        "valid-token"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_is_classified_without_command_details() {
    // exec keeps the sleeping program in the owned child, so cancellation
    // does not leave a shell's descendant behind.
    let mut config = command_config("/bin/sh", &["-c", "exec sleep 30"]);
    config.timeout_ms = NonZeroU64::new(25).unwrap();
    let error = run_provider_auth_command(&config).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(error.to_string(), "provider auth command timed out");
}
