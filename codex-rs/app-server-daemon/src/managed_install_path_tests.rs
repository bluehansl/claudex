use pretty_assertions::assert_eq;

#[test]
fn original_layouts_and_legacy_state_do_not_change_claudex_paths() {
    let home = tempfile::TempDir::new().unwrap();
    let expected = home.path().join("packages/claudex-app-server-daemon");
    for relative in [
        "packages/standalone/current",
        "packages/app-server-daemon/current",
    ] {
        let current = home.path().join(relative);
        std::fs::create_dir_all(current.join("bin")).unwrap();
        std::fs::write(current.join("bin/codex"), b"original").unwrap();
        let state = home.path().join("app-server-daemon");
        std::fs::create_dir_all(&state).unwrap();
        for file in ["daemon.pid", "app-server.pid", "app-server.stderr.log"] {
            std::fs::write(state.join(file), b"original").unwrap();
        }
        assert_eq!(super::package_root(home.path()), expected);
        assert_eq!(
            super::managed_codex_bin(home.path()),
            expected
                .join("current/bin")
                .join(super::managed_codex_file_name())
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn older_managed_binary_does_not_claim_updater_support() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::TempDir::new().expect("home");
    let binary = temp.path().join("codex");
    std::fs::write(&binary, b"#!/bin/sh\nexit 2\n").expect("older binary");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
        .expect("executable binary");
    assert!(!super::supports_daemon_update_loop(&binary).await);
    std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").expect("newer binary");
    assert!(super::supports_daemon_update_loop(&binary).await);
}
