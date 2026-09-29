//! 완전한 Claudex 패키지 복제와 원본 설치 격리 회귀 검증.
#![cfg(unix)]
use super::InstallMode;
use super::prepare_from_package;
use super::validate_package;
use crate::settings::DaemonSettings;
use pretty_assertions::assert_eq;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn daemon(home: &Path) -> crate::Daemon {
    let state = home.join(crate::STATE_DIR_NAME);
    crate::Daemon {
        log_diagnostics: false,
        socket_path: state.join("app-server.sock"),
        pid_file: state.join("daemon.pid"),
        update_pid_file: state.join("daemon-updater.pid"),
        operation_lock_file: state.join("daemon.lock"),
        settings_file: state.join("settings.json"),
        managed_codex_bin: crate::managed_install::managed_codex_bin(home),
    }
}

pub(crate) fn package(root: &Path, version: &str) -> PathBuf {
    for dir in ["bin", "codex-path", "codex-resources/nested"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let bin = root.join("bin/claudex");
    std::fs::write(&bin, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli {version}'; elif [ \"$4\" = --help ] || [ \"$3\" = --help ]; then exit 0; else exec sleep 30; fi\n")).unwrap();
    for file in [
        "bin/codex-code-mode-host",
        "codex-path/rg",
        "codex-resources/nested/runtime",
        "codex-resources/bwrap",
    ] {
        std::fs::write(root.join(file), b"runtime").unwrap();
        std::fs::set_permissions(root.join(file), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("codex-package.json"),
        serde_json::json!({
            "layoutVersion": 1, "variant": "claudex", "version": version,
            "target": super::platform_target().unwrap(), "entrypoint": "bin/claudex"
        })
        .to_string(),
    )
    .unwrap();
    bin
}

#[tokio::test]
async fn seeds_full_package_without_public_installer_marker() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    let source = temp.path().join("source");
    let bin = package(&source, "0.159.0");
    prepare_from_package(
        &daemon(&home),
        &DaemonSettings::default(),
        InstallMode::Missing,
        Some(&source),
        &bin,
        |_| Ok(true),
    )
    .await
    .unwrap();
    let root = crate::managed_install::package_root(&home);
    let selected = root.join("current").canonicalize().unwrap();
    assert_eq!(
        std::fs::read(selected.join("codex-resources/nested/runtime")).unwrap(),
        b"runtime"
    );
    assert!(!root.join("auto-update-version").exists());
    assert!(!selected.join("bin/codex").exists());
    validate_package(&selected).unwrap();
}

#[tokio::test]
async fn incomplete_source_fails_without_selecting_it() {
    let temp = tempfile::TempDir::new().unwrap();
    let source = temp.path().join("package");
    let bin = package(&source, "0.159.0");
    std::fs::remove_file(source.join("bin/codex-code-mode-host")).unwrap();
    let home = temp.path().join("home");
    let error = prepare_from_package(
        &daemon(&home),
        &DaemonSettings::default(),
        InstallMode::Missing,
        Some(&source),
        &bin,
        |_| Ok(true),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("bin/codex-code-mode-host"));
    assert!(
        !crate::managed_install::package_root(&home)
            .join("current")
            .exists()
    );
}

#[tokio::test]
async fn original_selection_and_state_are_never_adopted() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    let original = home.join("packages/standalone");
    let original_bin = package(&original.join("releases/old"), "0.150.0");
    std::os::unix::fs::symlink("releases/old", original.join("current")).unwrap();
    let state = home.join("app-server-daemon");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("app-server.pid"), b"original-pid").unwrap();
    std::fs::write(original.join("auto-update-version"), b"old").unwrap();
    let before = std::fs::read(&original_bin).unwrap();
    let source = temp.path().join("new");
    let bin = package(&source, "0.159.0");
    prepare_from_package(
        &daemon(&home),
        &DaemonSettings::default(),
        InstallMode::Missing,
        Some(&source),
        &bin,
        |_| Ok(true),
    )
    .await
    .unwrap();
    assert!(
        crate::managed_install::managed_codex_bin(&home)
            .canonicalize()
            .unwrap()
            .starts_with(
                crate::managed_install::package_root(&home)
                    .canonicalize()
                    .unwrap()
            )
    );
    assert_eq!(std::fs::read(original_bin).unwrap(), before);
    assert_eq!(
        std::fs::read(original.join("auto-update-version")).unwrap(),
        b"old"
    );
    assert_eq!(
        std::fs::read(state.join("app-server.pid")).unwrap(),
        b"original-pid"
    );
}

#[tokio::test]
async fn explicit_selection_requires_unchanged_cli_and_preserves_cancel() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    let source = temp.path().join("source");
    let bin = package(&source, "0.159.0");
    let settings = DaemonSettings::default();
    prepare_from_package(
        &daemon(&home),
        &settings,
        InstallMode::Missing,
        Some(&source),
        &bin,
        |_| Ok(true),
    )
    .await
    .unwrap();
    let root = crate::managed_install::package_root(&home);
    let mut previous = root.join("current").canonicalize().unwrap();
    let error = prepare_from_package(
        &daemon(&home),
        &settings,
        InstallMode::Replace,
        Some(&source),
        &bin,
        |_| {
            let mut bytes = std::fs::read(&bin)?;
            bytes.extend_from_slice(b"# changed during confirmation\n");
            std::fs::write(&bin, bytes)?;
            Ok(true)
        },
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("differs from the running executable")
    );
    assert_eq!(root.join("current").canonicalize().unwrap(), previous);
    for version in ["0.160.0", "0.158.0", "0.158.0", "0.161.0-alpha.1", "0.0.0"] {
        package(&source, version);
        std::fs::write(
            source.join("codex-resources/nested/runtime"),
            previous.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let before = std::fs::read(previous.join("bin/claudex")).unwrap();
        assert!(
            !prepare_from_package(
                &daemon(&home),
                &settings,
                InstallMode::Replace,
                Some(&source),
                &bin,
                |_| Ok(false)
            )
            .await
            .unwrap()
        );
        assert_eq!(root.join("current").canonicalize().unwrap(), previous);
        prepare_from_package(
            &daemon(&home),
            &settings,
            InstallMode::Replace,
            Some(&source),
            &bin,
            |_| Ok(true),
        )
        .await
        .unwrap();
        let selected = root.join("current").canonicalize().unwrap();
        assert_ne!(selected, previous);
        assert_eq!(
            std::fs::read(selected.join("bin/claudex")).unwrap(),
            std::fs::read(&bin).unwrap()
        );
        assert_eq!(std::fs::read(previous.join("bin/claudex")).unwrap(), before);
        assert!(!root.join("auto-update-version").exists());
        previous = selected;
    }
}

#[tokio::test]
async fn broken_selection_is_not_a_missing_installation() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    let source = temp.path().join("source");
    let bin = package(&source, "0.159.0");
    let current = crate::managed_install::package_root(&home).join("current");
    std::fs::create_dir_all(current.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("missing-release", &current).unwrap();
    let error = prepare_from_package(
        &daemon(&home),
        &DaemonSettings::default(),
        InstallMode::Missing,
        Some(&source),
        &bin,
        |_| panic!("broken installation must not request replacement"),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("repair the existing installation"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read_link(current).unwrap(),
        PathBuf::from("missing-release")
    );
}

#[test]
fn source_manifest_must_be_claudex_for_the_current_platform() {
    let temp = tempfile::TempDir::new().unwrap();
    for field in ["variant", "target", "entrypoint", "layoutVersion"] {
        package(temp.path(), "0.159.0");
        let path = temp.path().join("codex-package.json");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        metadata[field] = serde_json::json!("invalid");
        std::fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert!(validate_package(temp.path()).is_err(), "{field}");
    }
}

#[test]
fn package_tree_rejects_escaping_links() {
    let temp = tempfile::TempDir::new().unwrap();
    let source = temp.path().join("source");
    package(&source, "0.159.0");
    std::fs::write(temp.path().join("outside"), b"outside").unwrap();
    std::os::unix::fs::symlink("../../outside", source.join("bin/escape")).unwrap();
    assert!(super::package_tree(&source, None).is_err());
}
