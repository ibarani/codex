use super::PluginBackgroundTasks;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

#[test]
fn shutdown_cancels_and_joins_a_registered_worker() -> anyhow::Result<()> {
    let tasks = PluginBackgroundTasks::default();
    let completed = Arc::new(AtomicBool::new(/*v*/ false));
    let observed = Arc::clone(&completed);
    let (started, ready) = std::sync::mpsc::channel();
    tasks.spawn("plugin-cancellation-fixture", move |cancellation| {
        let _ = started.send(());
        while cancellation.check().is_ok() {
            std::thread::sleep(Duration::from_millis(/*millis*/ 1));
        }
        observed.store(/*val*/ true, Ordering::SeqCst);
    })?;
    ready.recv_timeout(Duration::from_secs(/*secs*/ 2))?;
    assert!(tasks.shutdown(Duration::from_secs(/*secs*/ 2)));
    assert!(completed.load(Ordering::SeqCst));
    assert!(
        tasks
            .threads
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned fixture"))?
            .is_empty()
    );
    Ok(())
}

#[test]
fn shutdown_rejects_new_worker_without_running_it() -> anyhow::Result<()> {
    let tasks = PluginBackgroundTasks::default();
    assert!(tasks.shutdown(Duration::ZERO));
    let ran = Arc::new(AtomicBool::new(/*v*/ false));
    let observed = Arc::clone(&ran);
    let result = tasks.spawn("rejected-plugin-fixture", move |_| {
        observed.store(/*val*/ true, Ordering::SeqCst)
    });
    assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::Interrupted));
    assert!(!ran.load(Ordering::SeqCst));
    Ok(())
}

#[test]
fn drain_timeout_retains_unfinished_worker_for_later_confirmation() -> anyhow::Result<()> {
    let tasks = PluginBackgroundTasks::default();
    let release = Arc::new(AtomicBool::new(/*v*/ false));
    let worker_release = Arc::clone(&release);
    tasks.spawn("blocked-plugin-fixture", move |_| {
        let deadline = Instant::now() + Duration::from_secs(/*secs*/ 2);
        while !worker_release.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(/*millis*/ 1));
        }
    })?;
    let drained_before_release = tasks.shutdown(Duration::ZERO);
    release.store(/*val*/ true, Ordering::SeqCst);
    let drained_after_release = tasks.shutdown(Duration::from_secs(/*secs*/ 2));
    assert!(!drained_before_release);
    assert!(drained_after_release);
    Ok(())
}

#[test]
fn completed_workers_are_reaped_before_new_admission() -> anyhow::Result<()> {
    let tasks = PluginBackgroundTasks::default();
    tasks.spawn("completed-plugin-fixture", |_| {})?;
    let deadline = Instant::now() + Duration::from_secs(/*secs*/ 2);
    loop {
        let finished = tasks
            .threads
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned fixture"))?[0]
            .is_finished();
        if finished {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "worker completion deadline");
        std::thread::yield_now();
    }
    tasks.spawn("next-plugin-fixture", |_| {})?;
    let retained = tasks
        .threads
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned fixture"))?
        .len();
    assert!(tasks.shutdown(Duration::from_secs(/*secs*/ 2)));
    assert_eq!(retained, 1);
    Ok(())
}

#[test]
fn cancelled_http_operation_does_not_start() -> anyhow::Result<()> {
    let tasks = PluginBackgroundTasks::default();
    let cancellation = super::PluginCancellation {
        receiver: Some(tasks.cancelled.subscribe()),
    };
    tasks.shutdown(Duration::ZERO);
    let mut ran = false;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(cancellation.run(async {
        ran = true;
        Ok(())
    }));
    assert!(result.is_err());
    assert!(!ran);
    Ok(())
}

#[cfg(unix)]
#[test]
fn plugin_shutdown_cancels_actual_owned_git_command() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let ready = directory.path().join("ready");
    let command_ready = ready.clone();
    let tasks = PluginBackgroundTasks::default();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    tasks.spawn("plugin-git-lifetime-fixture", move |cancellation| {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf ready > \"$1\"; sleep 30",
                "plugin-git-fixture",
            ])
            .arg(command_ready);
        let result = cancellation.git(command, "fixture Git", /*timeout*/ None);
        let _ = result_tx.send(result);
    })?;
    let deadline = Instant::now() + Duration::from_secs(/*secs*/ 2);
    while !ready.exists() {
        if Instant::now() >= deadline {
            tasks.shutdown(Duration::from_secs(/*secs*/ 2));
            anyhow::bail!("Git startup deadline");
        }
        std::thread::sleep(Duration::from_millis(/*millis*/ 1));
    }
    assert!(tasks.shutdown(Duration::from_secs(/*secs*/ 2)));
    assert!(
        matches!(result_rx.recv_timeout(Duration::from_secs(/*secs*/ 2))?, Err(error) if error == "fixture Git: Interrupted")
    );
    Ok(())
}

#[test]
fn cancellation_stops_an_in_flight_http_future() -> anyhow::Result<()> {
    let tasks = PluginBackgroundTasks::default();
    let cancellation = super::PluginCancellation {
        receiver: Some(tasks.cancelled.subscribe()),
    };
    let entered = Arc::new(AtomicBool::new(/*v*/ false));
    let observed = Arc::clone(&entered);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let operation = cancellation.run(async {
            observed.store(/*val*/ true, Ordering::SeqCst);
            std::future::pending::<Result<(), String>>().await
        });
        tokio::pin!(operation);
        assert!(futures::poll!(operation.as_mut()).is_pending());
        assert!(entered.load(Ordering::SeqCst));
        tasks.shutdown(Duration::ZERO);
        operation.await
    });
    assert_eq!(result, Err("plugin background task cancelled".to_string()));
    Ok(())
}
