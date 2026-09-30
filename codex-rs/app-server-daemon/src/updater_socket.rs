//! 긴 CODEX_HOME에서도 updater IPC 주소를 안정적으로 선택하고 본인 소켓만 정리한다.

use anyhow::Context;
use anyhow::Result;
use codex_uds::UnixListener;
use std::path::Path;
use std::path::PathBuf;

pub(super) fn path(pid_file: &Path) -> Result<PathBuf> {
    let legacy = pid_file.with_extension("sock");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::net::SocketAddr;

        let parent = legacy.parent().context("updater socket has no parent")?;
        // 최초 실행은 state 디렉터리 생성 전일 수 있다. HOME의 alias는 동일 주소로 정규화한다.
        let directory = match std::fs::canonicalize(parent) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::canonicalize(parent.parent().context("updater state has no home")?)?
                    .join(parent.file_name().context("updater state has no name")?)
            }
            Err(error) => return Err(error).context("failed to resolve updater socket directory"),
        };
        let canonical = directory.join(legacy.file_name().context("updater socket has no name")?);
        if SocketAddr::from_pathname(&canonical).is_ok() {
            return Ok(canonical);
        }
        let mut hash = blake3::Hasher::new();
        hash.update(b"claudex-daemon-updater\0");
        hash.update(canonical.as_os_str().as_bytes());
        let short =
            codex_uds::shared_daemon_socket_directory()?.join(hash.finalize().to_hex().as_str());
        SocketAddr::from_pathname(&short).context("protected updater socket path is too long")?;
        Ok(short)
    }
    #[cfg(not(unix))]
    {
        Ok(legacy)
    }
}

pub(super) async fn bind(socket: &Path) -> Result<(UnixListener, SocketGuard)> {
    let parent = socket.parent().context("updater socket has no parent")?;
    #[cfg(unix)]
    if parent == codex_uds::shared_daemon_socket_directory()? {
        codex_uds::prepare_shared_daemon_socket_directory()?;
    } else {
        codex_uds::prepare_private_socket_directory(parent).await?;
    }
    #[cfg(not(unix))]
    codex_uds::prepare_private_socket_directory(parent).await?;

    match codex_uds::is_stale_socket_path(socket).await {
        Ok(true) => tokio::fs::remove_file(socket).await?,
        Ok(false) => anyhow::bail!("updater socket path exists and is not a socket"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect updater socket"),
    }
    let listener = UnixListener::bind(socket)
        .await
        .with_context(|| format!("failed to bind updater socket {}", socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(socket)?;
        let guard = SocketGuard {
            path: socket.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        tokio::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).await?;
        Ok((listener, guard))
    }
    #[cfg(not(unix))]
    Ok((listener, SocketGuard {}))
}

pub(super) struct SocketGuard {
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[cfg(unix)]
impl Drop for SocketGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::FileTypeExt;
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        }) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[path = "updater_socket_tests.rs"]
mod tests;
