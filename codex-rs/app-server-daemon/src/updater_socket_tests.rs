//! 주소 호환, 홈 분리, alias, 기존 파일 및 교체된 소켓 보존 회귀.
#![cfg(unix)]

use super::*;
use pretty_assertions::assert_eq;

fn state(home: &Path) -> PathBuf {
    let directory = home.join(crate::STATE_DIR_NAME);
    std::fs::create_dir_all(&directory).unwrap();
    directory.join("daemon-updater.pid")
}

#[test]
fn short_home_preserves_the_legacy_endpoint() {
    let home = tempfile::Builder::new()
        .prefix("cu-")
        .tempdir_in("/tmp")
        .unwrap();
    let pid = state(home.path());
    let expected = pid
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
        .join("daemon-updater.sock");
    assert_eq!(path(&pid).unwrap(), expected);
}

#[test]
fn address_is_stable_before_the_state_directory_exists() {
    let home = tempfile::Builder::new()
        .prefix("cu-")
        .tempdir_in("/tmp")
        .unwrap();
    let pid = home
        .path()
        .join(crate::STATE_DIR_NAME)
        .join("daemon-updater.pid");
    let before = path(&pid).unwrap();
    state(home.path());
    assert_eq!(path(&pid).unwrap(), before);
}

#[test]
fn long_home_aliases_share_the_endpoint_without_cross_home_collisions() {
    let home = tempfile::Builder::new()
        .prefix(&"Long Orca home 한국어 ".repeat(5))
        .tempdir_in("/tmp")
        .unwrap();
    let pid = state(home.path());
    let socket = path(&pid).unwrap();
    let other = tempfile::Builder::new()
        .prefix(&"Long Orca home 한국어 ".repeat(5))
        .tempdir_in("/tmp")
        .unwrap();
    assert_ne!(path(&state(other.path())).unwrap(), socket);
    assert_ne!(
        path(&pid.with_file_name("app-server-updater.pid")).unwrap(),
        socket
    );
    assert!(socket.starts_with(codex_uds::shared_daemon_socket_directory().unwrap()));
    let aliases = tempfile::Builder::new()
        .prefix("cu-")
        .tempdir_in("/tmp")
        .unwrap();
    let alias = aliases.path().join("home");
    std::os::unix::fs::symlink(home.path(), &alias).unwrap();
    assert_eq!(
        path(&alias.join(crate::STATE_DIR_NAME).join("daemon-updater.pid")).unwrap(),
        socket
    );
}

#[tokio::test]
async fn bind_does_not_delete_regular_files_or_symlinks() {
    let home = tempfile::Builder::new()
        .prefix("cu-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = path(&state(home.path())).unwrap();
    std::fs::write(&socket, b"preserve me").unwrap();
    assert!(bind(&socket).await.is_err());
    assert_eq!(std::fs::read(&socket).unwrap(), b"preserve me");
    let link = socket.with_file_name("alias.sock");
    std::os::unix::fs::symlink(&socket, &link).unwrap();
    assert!(bind(&link).await.is_err());
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read(&socket).unwrap(), b"preserve me");
}

#[tokio::test]
async fn old_guard_does_not_remove_a_successor_socket() {
    let home = tempfile::Builder::new()
        .prefix("cu-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = path(&state(home.path())).unwrap();
    let (old_listener, old_guard) = bind(&socket).await.unwrap();
    let (new_listener, new_guard) = bind(&socket).await.unwrap();
    drop(old_guard);
    drop(old_listener);
    assert!(socket.exists());
    assert!(codex_uds::UnixStream::connect(&socket).await.is_ok());
    drop(new_listener);
    drop(new_guard);
    assert!(!socket.exists());
}
