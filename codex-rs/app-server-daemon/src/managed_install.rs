//! Claudex 전용 관리 패키지의 실행 파일을 선택하고 비교한다.

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs;
use tokio::process::Command;
use tokio::time::timeout;

/// New daemons own their packages, regardless of how the calling CLI was installed.
/// Preserve legacy launch state, including logs left after a daemon is stopped;
/// settings, installer selections, and lock files alone do not prove a prior launch.
pub(crate) fn package_root(codex_home: &Path) -> PathBuf {
    codex_home.join("packages/claudex-app-server-daemon")
}

/// 존재 여부와 별개로 Claudex 전용 실행 경로를 반환한다.
pub(crate) fn managed_codex_bin(codex_home: &Path) -> PathBuf {
    package_root(codex_home)
        .join("current/bin")
        .join(managed_codex_file_name())
}

/// 이름은 upstream 호출부와 호환되지만 자격은 Claudex 로컬 출처 정책으로 판단한다.
pub(crate) fn is_stable_standalone_release(codex_home: &Path, codex_bin: &Path) -> bool {
    crate::local_source::eligible(codex_home, codex_bin)
}

/// Older managed binaries can serve app-server requests without owning an updater.
pub(crate) async fn supports_daemon_update_loop(codex_bin: &Path) -> bool {
    supports_daemon_command(codex_bin, &["pid-update-loop", "--help"]).await
}

/// Probe an internal daemon command without running a long-lived process.
pub(crate) async fn supports_daemon_command(codex_bin: &Path, args: &[&str]) -> bool {
    let mut command = Command::new(codex_bin);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    timeout(
        Duration::from_secs(5),
        command
            .args(["app-server", "daemon"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .is_ok_and(|result| result.is_ok_and(|status| status.success()))
}

pub(crate) async fn resolved_managed_codex_bin(codex_bin: &Path) -> Result<PathBuf> {
    fs::canonicalize(codex_bin).await.with_context(|| {
        format!(
            "failed to resolve managed Codex binary {}",
            codex_bin.display()
        )
    })
}

pub(crate) async fn managed_codex_version(codex_bin: &Path) -> Result<String> {
    let mut command = Command::new(codex_bin);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let output = command
        .arg("--version")
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to invoke managed Codex binary {}",
                codex_bin.display()
            )
        })?;
    if !output.status.success() {
        return Err(anyhow!(
            "managed Codex binary {} exited with status {}",
            codex_bin.display(),
            output.status
        ));
    }

    let stdout = String::from_utf8(output.stdout).with_context(|| {
        format!(
            "managed Codex version was not utf-8: {}",
            codex_bin.display()
        )
    })?;
    parse_codex_version(&stdout)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecutableIdentity {
    digest: [u8; 32],
}

pub(crate) async fn executable_identity(executable: &Path) -> Result<ExecutableIdentity> {
    let executable = executable.to_path_buf();
    // Debug executables can be hundreds of MB. Stream the digest off the async
    // runtime instead of allocating the whole file and blocking a runtime thread.
    tokio::task::spawn_blocking(move || {
        std::fs::File::open(&executable)
            .and_then(executable_identity_from_reader)
            .with_context(|| format!("failed to read executable {}", executable.display()))
    })
    .await
    .context("executable identity task failed")?
}

pub(crate) fn executable_identity_from_reader(
    reader: impl std::io::Read,
) -> std::io::Result<ExecutableIdentity> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(reader)?;
    Ok(ExecutableIdentity {
        digest: *hasher.finalize().as_bytes(),
    })
}

fn managed_codex_file_name() -> &'static str {
    if cfg!(windows) {
        "claudex.exe"
    } else {
        "claudex"
    }
}

fn parse_codex_version(output: &str) -> Result<String> {
    let version = output
        .split_whitespace()
        .nth(1)
        .filter(|version| !version.is_empty())
        .ok_or_else(|| anyhow!("managed Codex version output was malformed"))?;
    Ok(version.to_string())
}

#[cfg(test)]
#[path = "managed_install_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "managed_install_path_tests.rs"]
mod path_tests;
