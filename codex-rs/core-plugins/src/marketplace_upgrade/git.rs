use crate::PluginGitMode;
use crate::background_tasks::PluginCancellation;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 30);

pub(super) fn git_remote_revision(
    codex_home: &Path,
    source: &str,
    ref_name: Option<&str>,
    mode: PluginGitMode,
    cancellation: &PluginCancellation,
) -> Result<String, String> {
    if let Some(ref_name) = ref_name
        && is_full_git_sha(ref_name)
    {
        return Ok(ref_name.to_string());
    }

    let ref_name = ref_name.unwrap_or("HEAD");
    let mut command = git_command(mode);
    let _trusted_repository = matches!(mode, PluginGitMode::Automatic)
        .then(|| crate::configure_trusted_git_repository(&mut command, codex_home))
        .transpose()?;
    command.arg("ls-remote").arg(source).arg(ref_name);
    let output = cancellation.git(
        command,
        "git ls-remote marketplace source",
        Some(GIT_TIMEOUT),
    )?;
    ensure_git_success(&output, "git ls-remote marketplace source")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(first_line) = stdout.lines().next() else {
        return Err("git ls-remote returned empty output for marketplace source".to_string());
    };
    let Some((revision, _)) = first_line.split_once('\t') else {
        return Err(format!(
            "unexpected git ls-remote output for marketplace source: {first_line}"
        ));
    };
    let revision = revision.trim();
    if revision.is_empty() {
        return Err("git ls-remote returned empty revision for marketplace source".to_string());
    }
    Ok(revision.to_string())
}

pub(super) fn clone_git_source(
    codex_home: &Path,
    source: &str,
    ref_name: Option<&str>,
    sparse_paths: &[String],
    destination: &Path,
    mode: PluginGitMode,
    cancellation: &PluginCancellation,
) -> Result<String, String> {
    let git_destination = git_path_arg(destination);
    let mut command = git_command(mode);
    let _trusted_repository = matches!(mode, PluginGitMode::Automatic)
        .then(|| crate::configure_trusted_git_repository(&mut command, codex_home))
        .transpose()?;
    if sparse_paths.is_empty() {
        command.arg("clone").arg(source).arg(&git_destination);
        let output =
            cancellation.git(command, "git clone marketplace source", Some(GIT_TIMEOUT))?;
        ensure_git_success(&output, "git clone marketplace source")?;
        if let Some(ref_name) = ref_name {
            let mut checkout = git_command(mode);
            checkout
                .arg("-C")
                .arg(&git_destination)
                .arg("checkout")
                .arg(ref_name);
            let output =
                cancellation.git(checkout, "git checkout marketplace ref", Some(GIT_TIMEOUT))?;
            ensure_git_success(&output, "git checkout marketplace ref")?;
        }
        return git_worktree_revision(&git_destination, mode, cancellation);
    }

    command
        .arg("clone")
        .arg("--filter=blob:none")
        .arg("--no-checkout")
        .arg(source)
        .arg(&git_destination);
    let output = cancellation.git(command, "git clone marketplace source", Some(GIT_TIMEOUT))?;
    ensure_git_success(&output, "git clone marketplace source")?;

    let mut sparse_checkout = git_command(mode);
    sparse_checkout
        .arg("-C")
        .arg(&git_destination)
        .arg("sparse-checkout")
        .arg("set")
        .args(sparse_paths);
    let output = cancellation.git(
        sparse_checkout,
        "git sparse-checkout marketplace source",
        Some(GIT_TIMEOUT),
    )?;
    ensure_git_success(&output, "git sparse-checkout marketplace source")?;

    let mut checkout = git_command(mode);
    checkout
        .arg("-C")
        .arg(&git_destination)
        .arg("checkout")
        .arg(ref_name.unwrap_or("HEAD"));
    let output = cancellation.git(checkout, "git checkout marketplace ref", Some(GIT_TIMEOUT))?;
    ensure_git_success(&output, "git checkout marketplace ref")?;
    git_worktree_revision(&git_destination, mode, cancellation)
}

fn git_worktree_revision(
    destination: &Path,
    mode: PluginGitMode,
    cancellation: &PluginCancellation,
) -> Result<String, String> {
    let mut command = git_command(mode);
    command
        .arg("-C")
        .arg(destination)
        .arg("rev-parse")
        .arg("HEAD");
    let output = cancellation.git(
        command,
        "git rev-parse marketplace revision",
        Some(GIT_TIMEOUT),
    )?;
    ensure_git_success(&output, "git rev-parse marketplace revision")?;

    let revision = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if revision.is_empty() {
        Err("git rev-parse returned empty revision for marketplace source".to_string())
    } else {
        Ok(revision)
    }
}

fn is_full_git_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn git_command(mode: PluginGitMode) -> Command {
    let mut command = mode.command(Path::new("git"));
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

#[cfg(windows)]
fn git_path_arg(path: &Path) -> PathBuf {
    strip_windows_verbatim_path_prefix(&path.to_string_lossy())
        .map(PathBuf::from)
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(not(windows))]
fn git_path_arg(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(any(windows, test))]
fn strip_windows_verbatim_path_prefix(path: &str) -> Option<String> {
    let stripped = path.strip_prefix(r"\\?\")?;
    let stripped = stripped
        .strip_prefix(r"UNC\")
        .map(|unc_path| format!(r"\\{unc_path}"))
        .unwrap_or_else(|| stripped.to_string());
    Some(stripped)
}

fn ensure_git_success(output: &std::process::Output, context: &str) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(format!("{context} failed with status {}", output.status))
    } else {
        Err(format!(
            "{context} failed with status {}: {stderr}",
            output.status
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::git_command;
    use super::is_full_git_sha;
    use super::strip_windows_verbatim_path_prefix;
    use pretty_assertions::assert_eq;
    use std::ffi::OsStr;

    #[test]
    fn full_git_sha_ref_is_already_a_remote_revision() {
        assert!(is_full_git_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_git_sha("main"));
        assert!(!is_full_git_sha("0123456"));
    }

    #[test]
    fn git_command_uses_path_lookup_with_stable_noninteractive_env() {
        let command = git_command(crate::PluginGitMode::Automatic);

        assert_eq!(command.get_program(), OsStr::new("git"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("-c"),
                OsStr::new(codex_git_utils::SAFE_BARE_REPOSITORY_CONFIG),
            ]
        );
        assert_eq!(
            command_env(&command, "GIT_OPTIONAL_LOCKS"),
            Some(Some(OsStr::new("0")))
        );
        assert_eq!(
            command_env(&command, "GIT_TERMINAL_PROMPT"),
            Some(Some(OsStr::new("0")))
        );
        assert_eq!(command_env(&command, "PATH"), None);
    }

    #[test]
    fn strips_windows_verbatim_disk_prefix_for_git() {
        assert_eq!(
            strip_windows_verbatim_path_prefix(r"\\?\C:\Users\alice\marketplace"),
            Some(r"C:\Users\alice\marketplace".to_string())
        );
    }

    #[test]
    fn strips_windows_verbatim_unc_prefix_for_git() {
        assert_eq!(
            strip_windows_verbatim_path_prefix(r"\\?\UNC\server\share\marketplace"),
            Some(r"\\server\share\marketplace".to_string())
        );
    }

    #[test]
    fn leaves_non_verbatim_path_without_rewrite() {
        assert_eq!(strip_windows_verbatim_path_prefix(r"C:\Users\alice"), None);
    }

    fn command_env<'a>(
        command: &'a std::process::Command,
        name: &str,
    ) -> Option<Option<&'a OsStr>> {
        command
            .get_envs()
            .find(|(key, _)| key == &OsStr::new(name))
            .map(|(_, value)| value)
    }
}
