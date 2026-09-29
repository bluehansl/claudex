#![cfg(unix)]
use super::*;
use crate::prepare_install::tests::package;
use pretty_assertions::assert_eq;

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let source = home.join("npm");
    package(&source, "1.0.0");
    remember(&home, &source, Selection::Explicit).unwrap();
    let root = crate::managed_install::package_root(&home);
    let initial = root.join("releases/initial");
    fs::create_dir_all(&initial).unwrap();
    crate::prepare_install::package_tree(&source, Some(&initial)).unwrap();
    select(&root, &initial).unwrap();
    (temp, source, root, initial)
}

#[tokio::test]
async fn synchronization_copies_full_package_and_preserves_no_op_selection() {
    let (_temp, source, root, initial) = fixture();
    sync(&root, &initial).await.unwrap();
    assert_eq!(root.join("current").canonicalize().unwrap(), initial);
    package(&source, "1.1.0");
    sync(&root, &initial).await.unwrap();
    let updated = root.join("current").canonicalize().unwrap();
    assert_ne!(updated, initial);
    assert_eq!(
        crate::prepare_install::package_tree(&updated, None).unwrap(),
        crate::prepare_install::package_tree(&source, None).unwrap()
    );
    assert!(initial.join("bin/claudex").is_file());
    assert!(!root.join("auto-update-version").exists());
}

#[tokio::test]
async fn helper_only_update_and_concurrent_retry_preserve_current() {
    let (_temp, source, root, initial) = fixture();
    fs::write(source.join("codex-resources/nested/runtime"), b"new").unwrap();
    let (a, b) = tokio::join!(sync(&root, &initial), sync(&root, &initial));
    assert_ne!(a.is_ok(), b.is_ok());
    let updated = root.join("current").canonicalize().unwrap();
    assert_ne!(updated, initial);
    assert_eq!(
        fs::read(updated.join("codex-resources/nested/runtime")).unwrap(),
        b"new"
    );
}

#[tokio::test]
async fn bad_source_never_changes_selected_package() {
    for failure in [
        "missing_helper",
        "wrong_product",
        "wrong_version",
        "escaping_link",
    ] {
        let (_temp, source, root, initial) = fixture();
        match failure {
            "missing_helper" => fs::remove_file(source.join("bin/codex-code-mode-host")).unwrap(),
            "wrong_product" | "wrong_version" => {
                let path = source.join("codex-package.json");
                let mut manifest: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                if failure == "wrong_product" {
                    manifest["variant"] = serde_json::json!("codex");
                } else {
                    manifest["version"] = serde_json::json!("9.9.9");
                }
                fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            }
            _ => {
                fs::write(root.join("outside"), b"outside").unwrap();
                std::os::unix::fs::symlink(root.join("outside"), source.join("bin/escape"))
                    .unwrap();
            }
        }
        assert!(sync(&root, &initial).await.is_err(), "{failure}");
        assert_eq!(root.join("current").canonicalize().unwrap(), initial);
    }
}

#[tokio::test]
async fn fresh_package_version_probe_allows_cold_launch() {
    let (_temp, source, root, initial) = fixture();
    let binary = package(&source, "1.1.0");
    fs::write(binary, b"#!/bin/sh\nsleep 6\necho codex-cli 1.1.0\n").unwrap();
    sync(&root, &initial).await.unwrap();
    assert_ne!(root.join("current").canonicalize().unwrap(), initial);
}

#[test]
fn registration_preserves_newer_source_unless_explicit_and_never_self_registers() {
    let (temp, source, root, initial) = fixture();
    let home = temp.path().canonicalize().unwrap();
    let older = home.join("older");
    package(&older, "0.9.0");
    remember(&home, &older, Selection::PreferNewest).unwrap();
    assert_eq!(read_source(&home).unwrap().package_dir, source);
    remember(&home, &initial, Selection::Explicit).unwrap();
    assert_eq!(read_source(&home).unwrap().package_dir, source);
    remember(&home, &older, Selection::Explicit).unwrap();
    assert_eq!(read_source(&home).unwrap().package_dir, older);
    assert!(eligible(&home, &root.join("current/bin/claudex")));
    assert!(!eligible(&home, &source.join("bin/claudex")));
}

#[test]
fn source_descriptor_requires_private_owned_regular_file() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, _source, _root, _initial) = fixture();
    let path = descriptor(temp.path());
    let bytes = fs::read(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_source(temp.path()).is_err());
    fs::remove_file(&path).unwrap();
    let other = temp.path().join("other");
    fs::write(&other, bytes).unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(other, &path).unwrap();
    assert!(read_source(temp.path()).is_err());
}
