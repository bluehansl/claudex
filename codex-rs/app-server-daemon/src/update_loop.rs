//! Owns scheduled and manual installs, daemon restarts, and updater replacement.

use std::path::Path;
#[cfg(unix)]
use std::process::Command as StdCommand;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_http_client::HttpClientFactory;
use futures::FutureExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use tokio::signal::unix::Signal;
#[cfg(unix)]
use tokio::signal::unix::SignalKind;
#[cfg(unix)]
use tokio::signal::unix::signal;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::sleep_until;

use crate::Daemon;
use crate::RestartIfRunningOutcome;
use crate::RestartMode;
use crate::managed_install::ExecutableIdentity;
use crate::managed_install::executable_identity;
use crate::managed_install::resolved_managed_codex_bin;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use crate::settings::DaemonSettings;
use crate::settings::UpdaterSettings;

#[path = "manual_update.rs"]
mod manual_update;

pub(crate) async fn request_manual_update(
    daemon: &Daemon,
    _http_client_factory: HttpClientFactory,
) -> Result<crate::UpdateOutput> {
    manual_update::request(daemon).await
}

const INITIAL_UPDATE_DELAY: Duration = Duration::from_secs(5 * 60);
const RESTART_RETRY_INTERVAL: Duration = Duration::from_millis(50);
pub(crate) async fn run(
    _http_client_factory: HttpClientFactory,
    restore_release: Option<String>,
) -> Result<()> {
    run_managed(
        &Daemon::from_environment()?,
        &current_updater_identity().await?,
        restore_release,
    )
    .await
}

async fn run_managed(
    daemon: &Daemon,
    running_updater_identity: &ExecutableIdentity,
    mut restore_release: Option<String>,
) -> Result<()> {
    #[cfg(unix)]
    let mut terminate =
        signal(SignalKind::terminate()).context("failed to install updater shutdown handler")?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let paths = daemon.backend_paths(&DaemonSettings::default());
        for backend in [
            crate::backend::pid_backend(paths.clone()),
            crate::backend::pid_update_loop_backend(paths),
        ] {
            // Inspection failures must preserve legacy records without blocking updates.
            let _ = backend.promote_legacy_identity().await;
        }
    }
    #[cfg(windows)]
    let updater = {
        // Updater ownership needs only paths, not settings that may be mid-edit.
        crate::backend::pid_update_loop_backend(daemon.backend_paths(&DaemonSettings::default()))
    };
    #[cfg(windows)]
    updater.wait_for_ownership().await?;
    #[cfg(windows)]
    let mut terminate = Signal;
    #[cfg(windows)]
    let _installer_job = crate::backend::windows::updater_job()?;
    let socket_path = daemon.manual_update_socket_path()?;
    let (bound_listener, _socket_guard) = crate::updater_socket::bind(&socket_path).await?;
    let mut listener = Some(bound_listener);
    #[cfg(windows)]
    updater.mark_ready().await?;
    let needs_managed_handoff =
        match resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await {
            Ok(managed_bin) => {
                executable_identity(&managed_bin).await.ok().as_ref()
                    != Some(running_updater_identity)
            }
            Err(_) => true,
        };
    let auto_update_enabled = UpdaterSettings::load(&daemon.settings_file)
        .await
        .map(|settings| settings.auto_update_enabled)
        .unwrap_or(true);
    let mut next_check = Instant::now()
        + if restore_release.is_some() || needs_managed_handoff || !auto_update_enabled {
            Duration::from_secs(15)
        } else {
            INITIAL_UPDATE_DELAY
        };
    let mut manual_handoff_pending = needs_managed_handoff;
    loop {
        tokio::select! {
            biased;
            _ = terminate.recv() => return Ok(()),
            connection = listener.as_mut().context("updater listener closed")?.accept() => {
                let connection = connection.context("failed to accept updater request")?;
                // Only a manual CLI launch under the operation lock supplies this
                // single-use authority. Ordinary socket requests cannot undo a pin.
                let restoration = restore_release.take();
                let trigger = restoration.as_deref().map_or(UpdateTrigger::Manual, UpdateTrigger::ExpectedSelection);
                let disposition = match manual_update::handle_request(connection, daemon, running_updater_identity, &mut terminate, trigger).await {
                    Ok(manual_update::RequestDisposition::Stop) => return Ok(()),
                    Ok(disposition) => disposition,
                    Err(_) => manual_update::RequestDisposition::Continue,
                };
                if UpdaterSettings::load(&daemon.settings_file)
                    .await
                    .is_ok_and(|settings| !settings.auto_update_enabled)
                {
                    // Drain requests queued during this one-shot update before exiting.
                    next_check = Instant::now() + Duration::from_millis(100);
                    continue;
                }
                if matches!(disposition, manual_update::RequestDisposition::Unchanged) {
                    continue;
                }
                manual_handoff_pending = true;
                next_check = Instant::now();
            }
            _ = sleep_until(next_check) => {
                // The authorizing CLI may have exited before sending its request.
                if restore_release.is_some() {
                    return Ok(());
                }
                match UpdaterSettings::load(&daemon.settings_file).await {
                    Ok(settings) if !settings.auto_update_enabled => return Ok(()),
                    Err(_) => {
                        next_check = Instant::now() + Duration::from_secs(60);
                        continue;
                    }
                    Ok(_) => {}
                }
                // Failed successor cleanup leaves its PID published. The predecessor
                // must stop instead of installing again without ownership.
                #[cfg(windows)]
                updater.wait_for_ownership().await?;
                if manual_handoff_pending {
                    if !daemon.is_stable_standalone_release()? {
                        if !daemon.has_latest_selection_marker() {
                            return Ok(());
                        }
                        next_check = Instant::now() + Duration::from_secs(30);
                        continue;
                    }
                    match adopt_managed_updater(daemon, running_updater_identity, &mut listener).await {
                        Ok(UpdateLoopControl::Stop) => return Ok(()),
                        Ok(UpdateLoopControl::Continue) => {
                            manual_handoff_pending = false;
                            let Some(delay) = next_update_delay(daemon).await else {
                                return Ok(());
                            };
                            next_check = Instant::now() + delay;
                        }
                        Err(err) => {
                            if listener.is_none() {
                                return Err(err);
                            }
                            eprintln!("warning: failed to refresh managed updater: {err:#}");
                            next_check = Instant::now() + Duration::from_secs(30);
                        }
                    }
                    continue;
                }
                match update_once(daemon, running_updater_identity, &mut terminate, UpdateTrigger::Scheduled).await {
                    Ok((UpdateLoopControl::Continue, Some(_))) => {
                        manual_handoff_pending = true;
                        next_check = Instant::now();
                        continue;
                    }
                    Ok((UpdateLoopControl::Continue, None)) | Err(_) => {}
                    Ok((UpdateLoopControl::Stop, _)) => return Ok(()),
                }
                let Some(delay) = next_update_delay(daemon).await else {
                    return Ok(());
                };
                next_check = Instant::now() + delay;
            }
        }
    }
}

async fn next_update_delay(daemon: &Daemon) -> Option<Duration> {
    match UpdaterSettings::load(&daemon.settings_file).await {
        Ok(settings) if !settings.auto_update_enabled => None,
        Ok(settings) => Some(settings.update_interval(Duration::from_secs(60))),
        Err(_) => Some(Duration::from_secs(60)),
    }
}

async fn adopt_managed_updater(
    daemon: &Daemon,
    running_identity: &ExecutableIdentity,
    listener: &mut Option<codex_uds::UnixListener>,
) -> Result<UpdateLoopControl> {
    let managed_bin = resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await?;
    if executable_identity(&managed_bin).await? == *running_identity {
        return Ok(UpdateLoopControl::Continue);
    }
    if !crate::managed_install::supports_daemon_update_loop(&managed_bin).await {
        return Ok(UpdateLoopControl::Stop);
    }
    #[cfg(unix)]
    {
        let _ = listener;
        reexec_managed_updater(&managed_bin).map(|_| UpdateLoopControl::Stop)
    }
    #[cfg(windows)]
    {
        let replacement = crate::backend::pid_update_loop_backend(
            daemon.backend_paths_with_bin(&daemon.load_settings().await?, &managed_bin),
        );
        listener.take();
        if let Err(err) = replacement.replace_current_updater().await {
            // A failed replacement may still own the PID if its cleanup could
            // not terminate the successor. Never reopen our request socket then.
            replacement.wait_for_ownership().await?;
            let socket_path = daemon.manual_update_socket_path()?;
            if socket_path.exists() {
                tokio::fs::remove_file(&socket_path).await?;
            }
            *listener = Some(codex_uds::UnixListener::bind(&socket_path).await?);
            return Err(err);
        }
        Ok(UpdateLoopControl::Stop)
    }
}

async fn sleep_or_terminate(duration: Duration, terminate: &mut Signal) -> bool {
    tokio::select! {
        _ = sleep(duration) => false,
        _ = terminate.recv() => true,
    }
}

enum UpdateLoopControl {
    Continue,
    Stop,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateTrigger<'a> {
    Scheduled,
    Manual,
    ExpectedSelection(&'a str),
}

async fn update_once(
    daemon: &Daemon,
    running_updater_identity: &ExecutableIdentity,
    terminate: &mut Signal,
    trigger: UpdateTrigger<'_>,
) -> Result<(UpdateLoopControl, Option<RestartIfRunningOutcome>)> {
    if trigger == UpdateTrigger::Scheduled
        && !UpdaterSettings::load(&daemon.settings_file)
            .await?
            .auto_update_enabled
    {
        return Ok((UpdateLoopControl::Stop, None));
    }
    if release_selection_unstable(daemon, trigger)? {
        // An installer can be between changing current and publishing its
        // latest-channel marker. Retry after the interval instead of exiting.
        return Ok((UpdateLoopControl::Continue, None));
    }
    let (package_root, previous_selection, previous_release) = selected_release(daemon)?;
    let codex_home = package_root
        .parent()
        .and_then(Path::parent)
        .context("daemon package root has no Codex home")?;
    if let UpdateTrigger::ExpectedSelection(expected) = trigger {
        anyhow::ensure!(
            previous_release == expected,
            "daemon selection changed; retry the update"
        );
    }
    anyhow::ensure!(
        crate::managed_install::package_root(codex_home) == package_root,
        "daemon package root changed"
    );
    tokio::select! {
        result = crate::local_source::sync(&package_root, &previous_selection) => result?,
        _ = terminate.recv() => return Ok((UpdateLoopControl::Stop, None)),
    }
    anyhow::ensure!(
        trigger == UpdateTrigger::Scheduled || daemon.is_stable_standalone_release()?,
        "local synchronization did not select a valid Claudex package; retry the update"
    );
    if release_selection_unstable(daemon, trigger)? {
        return Ok((UpdateLoopControl::Continue, None));
    }

    let managed_codex_bin =
        resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await?;
    let restart_mode = match trigger {
        // The package can contain different resources even when its CLI binary
        // is identical. A release change must also replace the running process.
        UpdateTrigger::Scheduled | UpdateTrigger::Manual | UpdateTrigger::ExpectedSelection(_)
            if selected_release(daemon)?.1 != previous_selection =>
        {
            RestartMode::Always
        }
        UpdateTrigger::Manual | UpdateTrigger::ExpectedSelection(_) => {
            RestartMode::IfBinaryOrVersionChanged
        }
        UpdateTrigger::Scheduled
            if executable_identity(&managed_codex_bin).await? != *running_updater_identity =>
        {
            RestartMode::Always
        }
        UpdateTrigger::Scheduled => RestartMode::IfVersionChanged,
    };

    loop {
        if terminate.recv().now_or_never().flatten().is_some() {
            return Ok((UpdateLoopControl::Stop, None));
        }
        match daemon
            .try_restart_if_running(restart_mode, &managed_codex_bin)
            .await?
        {
            RestartIfRunningOutcome::Busy => {
                if sleep_or_terminate(RESTART_RETRY_INTERVAL, terminate).await {
                    return Ok((UpdateLoopControl::Stop, None));
                }
            }
            RestartIfRunningOutcome::Restarted => {
                return Ok((
                    UpdateLoopControl::Continue,
                    Some(RestartIfRunningOutcome::Restarted),
                ));
            }
            RestartIfRunningOutcome::NotRunning => {
                return Ok((
                    UpdateLoopControl::Continue,
                    Some(RestartIfRunningOutcome::NotRunning),
                ));
            }
            RestartIfRunningOutcome::AlreadyCurrent
                if trigger != UpdateTrigger::Scheduled
                    && restart_mode == RestartMode::IfBinaryOrVersionChanged =>
            {
                anyhow::ensure!(
                    daemon.is_stable_standalone_release()?
                        && resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await?
                            == managed_codex_bin,
                    "managed daemon changed during the update; retry"
                );
                return Ok((
                    UpdateLoopControl::Continue,
                    Some(RestartIfRunningOutcome::AlreadyCurrent),
                ));
            }
            RestartIfRunningOutcome::NotReady | RestartIfRunningOutcome::AlreadyCurrent => {
                anyhow::ensure!(
                    trigger == UpdateTrigger::Scheduled,
                    "managed daemon could not restart; retry when it is ready"
                );
                return Ok((
                    if daemon.is_stable_standalone_release()? {
                        UpdateLoopControl::Continue
                    } else {
                        UpdateLoopControl::Stop
                    },
                    None,
                ));
            }
        }
    }
}

fn release_selection_unstable(daemon: &Daemon, trigger: UpdateTrigger<'_>) -> Result<bool> {
    if daemon.is_stable_standalone_release()?
        || matches!(trigger, UpdateTrigger::ExpectedSelection(_))
            && manual_update::supported(daemon)?
    {
        return Ok(false);
    }
    anyhow::ensure!(
        trigger == UpdateTrigger::Scheduled,
        "Claudex daemon selection changed during the update"
    );
    Ok(true)
}

fn selected_release(daemon: &Daemon) -> Result<(std::path::PathBuf, std::path::PathBuf, String)> {
    let home = daemon
        .settings_file
        .parent()
        .and_then(Path::parent)
        .context("daemon settings path has no Codex home")?;
    let root = crate::managed_install::package_root(home);
    let release = std::fs::canonicalize(root.join("current"))?;
    let name = release
        .file_name()
        .context("managed release has no name")?
        .to_string_lossy()
        .into_owned();
    Ok((root, release, name))
}

async fn current_updater_identity() -> Result<ExecutableIdentity> {
    let current_exe =
        std::env::current_exe().context("failed to resolve current updater executable")?;
    executable_identity(&current_exe).await
}

#[cfg(unix)]
pub(crate) fn reexec_managed_updater(managed_codex_bin: &std::path::Path) -> Result<()> {
    let err = StdCommand::new(managed_codex_bin)
        .args(["app-server", "daemon", "pid-update-loop"])
        .exec();
    Err(err).with_context(|| {
        format!(
            "failed to replace updater with managed Codex binary {}",
            managed_codex_bin.display()
        )
    })
}

#[cfg(test)]
#[path = "update_loop_tests.rs"]
mod tests;

#[cfg(windows)]
struct Signal;

#[cfg(windows)]
impl Signal {
    async fn recv(&mut self) -> Option<()> {
        // An unreadable control path must stop the updater rather than disable shutdown.
        let _ = codex_app_server_transport::daemon_shutdown_signal().await;
        Some(())
    }
}
