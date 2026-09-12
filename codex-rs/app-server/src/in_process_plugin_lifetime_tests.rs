//! Actual in-process app-server ownership of an admitted plugin Git worker.

use super::InProcessStartArgs;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeParams;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_exec_server::EnvironmentManager;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

const CHILD_ROOT: &str = "CODEX_TEST_PLUGIN_LIFETIME_ROOT";
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
enum Shutdown {
    Graceful,
    RuntimeDrop,
}

impl Shutdown {
    fn test_name(self) -> &'static str {
        match self {
            Self::Graceful => "in_process_shutdown_drains_admitted_plugin_git",
            Self::RuntimeDrop => "in_process_runtime_drop_stops_admitted_plugin_git",
        }
    }
}

#[test]
fn in_process_shutdown_drains_admitted_plugin_git() -> anyhow::Result<()> {
    isolated_case(Shutdown::Graceful)
}

#[test]
fn in_process_runtime_drop_stops_admitted_plugin_git() -> anyhow::Result<()> {
    isolated_case(Shutdown::RuntimeDrop)
}

fn isolated_case(shutdown: Shutdown) -> anyhow::Result<()> {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        return run_child(Path::new(&root), shutdown);
    }
    let fixture = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(/*mode*/ 0o700))
        .tempdir()?;
    let root = fixture.path().canonicalize()?;
    let home = root.join("home");
    let temporary = root.join("temporary");
    std::fs::create_dir(&home)?;
    std::fs::create_dir(&temporary)?;
    std::fs::write(home.join("config.toml"), "[features]\nplugins = true\n")?;
    let helper = root.join("git-ssh-fixture");
    std::fs::write(
        &helper,
        r#"#!/bin/sh
fixture=${0%/*}
printf '%s\n' "$$" > "$fixture/helper.pid.tmp"
mv "$fixture/helper.pid.tmp" "$fixture/helper.pid"
attempts=0
while [ -d "$fixture" ] && [ ! -f "$fixture/release" ] && [ "$attempts" -lt 1000 ]; do
    sleep 0.02
    attempts=$((attempts + 1))
done
printf fallback > "$fixture/fallback"
exit 1
"#,
    )?;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(/*mode*/ 0o700))?;
    let git_config = root.join("gitconfig");
    std::fs::write(
        &git_config,
        "[url \"ssh://plugin-lifetime.invalid/curated\"]\n    insteadOf = https://github.com/openai/plugins.git\n",
    )?;
    // Git still uses its installed binary and normal curated startup path. Its
    // SSH transport is this finite owned helper: no SSH client or network call.
    let helper_text = helper
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("fixture path is not UTF-8"))?;
    let ssh_command = format!("'{}'", helper_text.replace('\'', "'\"'\"'"));
    let test_module = module_path!()
        .split_once("::")
        .ok_or_else(|| anyhow::anyhow!("missing test module prefix"))?
        .1;
    let mut child = Command::new(std::env::current_exe()?);
    child
        .args([
            "--exact",
            &format!("{test_module}::{}", shutdown.test_name()),
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .env(CHILD_ROOT, &root)
        .env("HOME", &home)
        .env("CODEX_HOME", &home)
        .env("TMPDIR", &temporary)
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_GLOBAL", &git_config)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_SSH_COMMAND", ssh_command)
        .env("GIT_SSH_VARIANT", "ssh")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env("RUST_MIN_STACK", "8388608")
        .stdin(Stdio::null());
    // Preserve only loader paths and the existing runner's attribution keys.
    // No provider, Git credential, shell startup or user configuration is copied.
    for name in ["LD_LIBRARY_PATH", "NEXTEST_TEST_NAME", "NEXTEST_BINARY_ID"] {
        if let Some(value) = std::env::var_os(name) {
            child.env(name, value);
        }
    }
    let status = child.status()?;
    anyhow::ensure!(status.success(), "isolated plugin lifetime case failed");
    anyhow::ensure!(
        std::fs::read(root.join("passed"))? == shutdown.test_name().as_bytes(),
        "test child did not complete the actual ownership assertions"
    );
    Ok(())
}

fn run_child(root: &Path, shutdown: Shutdown) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let started = runtime.block_on(async {
        let home = root.join("home");
        let loader_overrides = LoaderOverrides::without_managed_config_for_tests();
        let config = Arc::new(
            ConfigBuilder::default()
                .codex_home(home.clone())
                .fallback_cwd(Some(home))
                .loader_overrides(loader_overrides.clone())
                .build()
                .await?,
        );
        anyhow::ensure!(
            config.plugins_config_input().plugins_enabled,
            "startup disabled"
        );
        let client = super::start(InProcessStartArgs {
            arg0_paths: Arg0DispatchPaths::default(),
            config,
            cli_overrides: Vec::new(),
            loader_overrides,
            strict_config: false,
            cloud_config_bundle: CloudConfigBundleLoader::default(),
            thread_config_loader: Arc::new(codex_config::NoopThreadConfigLoader),
            feedback: CodexFeedback::new(),
            log_db: None,
            state_db: None,
            environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
            config_warnings: Vec::new(),
            session_source: SessionSource::Cli,
            enable_codex_api_key_env: false,
            initialize: InitializeParams {
                client_info: ClientInfo {
                    name: "plugin-lifetime-test".to_string(),
                    title: None,
                    version: "0.0.0".to_string(),
                },
                capabilities: None,
            },
            channel_capacity: super::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
        })
        .await?;
        let deadline = Instant::now() + READY_TIMEOUT;
        let identity = loop {
            if let Ok(record) = std::fs::read_to_string(root.join("helper.pid"))
                && record.ends_with('\n')
            {
                let pid: u32 = record.trim().parse()?;
                anyhow::ensure!(pid > 0, "invalid fixture PID");
                let (live, start_ticks) = process_state(pid)?
                    .ok_or_else(|| anyhow::anyhow!("helper exited before observation"))?;
                anyhow::ensure!(live, "helper was not live before shutdown");
                anyhow::ensure!(plugin_worker_count()? > 0, "admitted plugin worker missing");
                break (pid, start_ticks);
            }
            anyhow::ensure!(Instant::now() < deadline, "plugin Git readiness deadline");
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        };
        Ok::<_, anyhow::Error>((client, identity))
    });
    let (client, identity) = match started {
        Ok(started) => started,
        Err(error) => {
            drop(runtime);
            std::fs::write(root.join("release"), "release")?;
            return Err(error);
        }
    };
    let outcome = match shutdown {
        Shutdown::Graceful => {
            let result = runtime.block_on(client.shutdown());
            // This assertion runs before dropping the Tokio runtime, so normal
            // cleanup cannot pass solely because runtime teardown killed tasks.
            let stopped = result
                .map_err(anyhow::Error::from)
                .and_then(|()| wait_stopped(identity));
            drop(runtime);
            stopped
        }
        Shutdown::RuntimeDrop => {
            drop(client);
            // No runtime polling after the handle closes: graceful routing has
            // no opportunity to run; the actual MessageProcessor guard must act.
            drop(runtime);
            wait_stopped(identity)
        }
    };
    let fallback_used = root.join("fallback").exists();
    // Failure cleanup uses only the owned fixture's release file. Never signal
    // a saved test PID; the original group owner alone performs termination.
    std::fs::write(root.join("release"), "release")?;
    if outcome.is_err() {
        let _ = wait_stopped(identity);
    }
    outcome?;
    anyhow::ensure!(
        !fallback_used,
        "fixture stopped through fallback before acceptance"
    );
    std::fs::write(root.join("passed"), shutdown.test_name())?;
    Ok(())
}

fn process_state(pid: u32) -> anyhow::Result<Option<(bool, u64)>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let tail = stat
        .rsplit_once(')')
        .ok_or_else(|| anyhow::anyhow!("invalid fixture process stat"))?
        .1;
    let fields = tail.split_whitespace().collect::<Vec<_>>();
    let live = !matches!(fields.first().copied(), Some("Z" | "X"));
    let start_ticks = fields
        .get(19)
        .ok_or_else(|| anyhow::anyhow!("missing fixture process identity"))?
        .parse()?;
    Ok(Some((live, start_ticks)))
}

fn wait_stopped((pid, start_ticks): (u32, u64)) -> anyhow::Result<()> {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        let helper_live =
            process_state(pid)?.is_some_and(|(live, observed)| live && observed == start_ticks);
        if !helper_live && plugin_worker_count()? == 0 {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "plugin worker or original helper remains live"
        );
        std::thread::sleep(Duration::from_millis(/*millis*/ 10));
    }
}

fn plugin_worker_count() -> anyhow::Result<usize> {
    let mut count = 0;
    for entry in std::fs::read_dir("/proc/self/task")? {
        let name = match std::fs::read_to_string(entry?.path().join("comm")) {
            Ok(name) => name,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        // Linux task names retain the first 15 bytes. These are the three
        // source-bound worker names; no ambient command or environment is read.
        if matches!(
            name.trim_end(),
            "plugins-curated" | "plugins-marketp" | "plugins-non-cur"
        ) {
            count += 1;
        }
    }
    Ok(count)
}
