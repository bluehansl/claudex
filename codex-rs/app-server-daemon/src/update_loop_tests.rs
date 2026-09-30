//! 로컬 패키지 갱신에서도 upstream의 재시작·큐·설정 보존 동작을 검증한다.
#![cfg(unix)]
use crate::Daemon;
use crate::UpdateOutput;
use crate::UpdateStatus;
use crate::managed_install::executable_identity;
use crate::managed_install::executable_identity_from_reader;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tempfile::TempDir;

fn manual_update_daemon(home: &TempDir) -> (Daemon, String) {
    let source = home.path().join("npm-source");
    crate::prepare_install::tests::package(&source, "1.0.0");
    crate::local_source::remember(
        home.path(),
        &source,
        crate::local_source::Selection::Explicit,
    )
    .unwrap();
    let root = crate::managed_install::package_root(home.path());
    let name = "local-initial";
    let release = root.join("releases").join(name);
    std::fs::create_dir_all(&release).unwrap();
    crate::prepare_install::package_tree(&source, Some(&release)).unwrap();
    crate::local_source::select(&root, &release).unwrap();
    (
        crate::prepare_install::tests::daemon(home.path()),
        name.to_string(),
    )
}

#[cfg(unix)]
fn test_terminate() -> tokio::signal::unix::Signal {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install test signal handler")
}

#[tokio::test]
async fn long_home_uses_a_bindable_updater_socket() {
    use std::os::unix::fs::MetadataExt;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let home = tempfile::Builder::new()
        .prefix(&"Claudex Orca runtime-home 한국어 ".repeat(4))
        .tempdir_in("/tmp")
        .expect("long home");
    let (daemon, _) = manual_update_daemon(&home);
    let legacy = daemon.update_pid_file.with_extension("sock");
    assert!(std::os::unix::net::SocketAddr::from_pathname(&legacy).is_err());
    let path = daemon.manual_update_socket_path().expect("updater path");
    assert!(
        std::os::unix::net::SocketAddr::from_pathname(&path).is_ok(),
        "updater must use a bindable socket even with a long CODEX_HOME"
    );
    let (mut listener, guard) = crate::updater_socket::bind(&path)
        .await
        .expect("bind updater");
    assert_eq!(
        std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    let reply = UpdateOutput {
        status: UpdateStatus::NoUpdate,
        managed_codex_path: daemon.managed_codex_bin.clone(),
        installed_version: Some("1.0.0".into()),
        running_version: Some("1.0.0".into()),
        message: "already current".into(),
    };
    let expected = reply.clone();
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        let mut request = [0; 7];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"update\n");
        stream
            .write_all(&serde_json::to_vec(&Ok::<_, String>(reply)).unwrap())
            .await
            .unwrap();
    });
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        super::manual_update::request(&daemon),
    )
    .await
    .expect("manual request must not wait for the old long address")
    .unwrap();
    assert_eq!(result, expected);
    server.await.unwrap();
    drop(guard);
    assert!(!path.exists(), "owned protected socket must be cleaned up");
}

#[tokio::test]
async fn updater_failure_reports_the_actual_log_without_waiting_for_timeout() {
    let home = tempfile::Builder::new()
        .prefix("cd-")
        .tempdir_in("/tmp")
        .unwrap();
    let (daemon, _) = manual_update_daemon(&home);
    let path = daemon.manual_update_socket_path().unwrap();
    let worker = crate::backend::PidBackend::new_update_loop(
        daemon.managed_codex_bin.clone(),
        daemon.update_pid_file.clone(),
        None,
    );
    std::fs::write(
        daemon.update_pid_file.with_extension("stderr.log"),
        "Error: path must be shorter than SUN_LEN\n",
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        super::manual_update::wait_for_updater(&daemon, &worker, &path),
    )
    .await
    .expect("exited worker must fail promptly");
    let message = result.err().expect("missing worker").to_string();
    assert!(message.contains("exited before accepting"));
    assert!(message.contains("path must be shorter than SUN_LEN"));
    assert!(message.contains("daemon-updater.stderr.log"));
}

#[cfg(unix)]
#[tokio::test]
async fn manual_request_retries_after_updater_replacement() {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let home = tempfile::Builder::new()
        .prefix("cd-")
        .tempdir_in("/tmp")
        .expect("home");
    let (daemon, _) = manual_update_daemon(&home);
    let socket_path = daemon.manual_update_socket_path().expect("updater path");
    codex_uds::prepare_private_socket_directory(socket_path.parent().expect("socket parent"))
        .await
        .expect("socket directory");
    let mut listener = codex_uds::UnixListener::bind(&socket_path)
        .await
        .expect("old updater socket");
    let expected = UpdateOutput {
        status: UpdateStatus::NoUpdate,
        managed_codex_path: daemon.managed_codex_bin.clone(),
        installed_version: None,
        running_version: None,
        message: "already current".to_string(),
    };
    let reply = expected.clone();
    let server = tokio::spawn(async move {
        let mut old = listener.accept().await.expect("first connection");
        let mut request = [0; 7];
        old.read_exact(&mut request).await.expect("first request");
        drop(old);
        drop(listener);
        tokio::fs::remove_file(&socket_path)
            .await
            .expect("remove old socket");
        let mut successor = codex_uds::UnixListener::bind(&socket_path)
            .await
            .expect("successor socket");
        let mut connection = successor.accept().await.expect("retried connection");
        connection
            .read_exact(&mut request)
            .await
            .expect("retried request");
        connection
            .write_all(&serde_json::to_vec(&Ok::<_, String>(reply)).expect("serialize response"))
            .await
            .expect("send response");
    });
    assert_eq!(
        super::manual_update::request(&daemon)
            .await
            .expect("request survives handoff"),
        expected
    );
    server.await.expect("replacement task");
}

#[cfg(unix)]
#[tokio::test]
async fn manual_request_recovers_when_one_shot_updater_exits() {
    use tokio::io::AsyncReadExt;

    let home = tempfile::Builder::new()
        .prefix("cd-")
        .tempdir_in("/tmp")
        .expect("home");
    let (daemon, _) = manual_update_daemon(&home);
    let socket_path = daemon.manual_update_socket_path().expect("updater path");
    codex_uds::prepare_private_socket_directory(socket_path.parent().expect("socket parent"))
        .await
        .expect("socket directory");
    let mut listener = codex_uds::UnixListener::bind(&socket_path)
        .await
        .expect("one-shot updater socket");
    let server = tokio::spawn(async move {
        let mut connection = listener.accept().await.expect("request connection");
        let mut request = [0; 7];
        connection.read_exact(&mut request).await.expect("request");
        drop(connection);
        drop(listener);
        tokio::fs::remove_file(socket_path)
            .await
            .expect("remove exited updater socket");
    });
    // Without the selected executable, the startup path reports unsupported. A
    // retry that only waits for a successor would time out instead.
    std::fs::remove_file(&daemon.managed_codex_bin).expect("remove selected binary");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        super::manual_update::request(&daemon),
    )
    .await
    .expect("retry should return to normal startup")
    .expect("unsupported response");
    assert_eq!(result.status, UpdateStatus::Unsupported);
    server.await.expect("updater task");
}

#[cfg(unix)]
#[tokio::test]
async fn unsupported_request_preserves_updater_schedule() {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let home = tempfile::Builder::new()
        .prefix("cd-")
        .tempdir_in("/tmp")
        .expect("home");
    let (daemon, _) = manual_update_daemon(&home);
    let daemon = std::sync::Arc::new(daemon);
    let identity = executable_identity(&daemon.managed_codex_bin)
        .await
        .expect("updater identity");
    let socket_path = daemon.manual_update_socket_path().expect("updater path");
    let updater_daemon = std::sync::Arc::clone(&daemon);
    let worker = tokio::spawn(async move {
        super::run_managed(&updater_daemon, &identity, /*restore_release*/ None).await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !socket_path.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "updater did not listen"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // An external installer can pin while this ordinary worker still listens.
    // Raw IPC must not gain the manual CLI's authority to restore production.
    let marker = home.path().join(crate::STATE_DIR_NAME).join("source.json");
    let previous_marker = std::fs::read(&marker).unwrap();
    std::fs::remove_file(&marker).unwrap();
    let mut pinned = codex_uds::UnixStream::connect(&socket_path).await.unwrap();
    pinned.write_all(b"update\n").await.unwrap();
    let mut response = Vec::new();
    pinned.read_to_end(&mut response).await.unwrap();
    let response: Result<UpdateOutput, String> = serde_json::from_slice(&response).unwrap();
    assert_eq!(response.unwrap().status, UpdateStatus::Unsupported);
    assert!(!marker.exists());
    std::fs::write(&marker, previous_marker).unwrap();
    std::fs::set_permissions(
        &marker,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )
    .unwrap();
    std::fs::remove_file(&daemon.managed_codex_bin).expect("remove selected binary");
    let mut malformed = codex_uds::UnixStream::connect(&socket_path)
        .await
        .expect("connect malformed request");
    malformed
        .write_all(b"upd")
        .await
        .expect("send partial request");
    malformed.shutdown().await.expect("disconnect request");
    let mut discarded = Vec::new();
    malformed
        .read_to_end(&mut discarded)
        .await
        .expect("rejected request");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!worker.is_finished(), "malformed request stopped updater");
    assert_eq!(
        super::manual_update::request(&daemon)
            .await
            .expect("unsupported response")
            .status,
        UpdateStatus::Unsupported
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!worker.is_finished(), "unsupported request stopped updater");
    worker.abort();
}

#[cfg(unix)]
async fn test_control_server(
    daemon: &Daemon,
    home: &std::path::Path,
) -> tokio::task::JoinHandle<()> {
    use futures::SinkExt;
    use futures::StreamExt;
    std::fs::create_dir_all(daemon.socket_path.parent().expect("socket parent"))
        .expect("socket directory");
    let mut listener = codex_uds::UnixListener::bind(&daemon.socket_path)
        .await
        .expect("control listener");
    let codex_home = home.to_path_buf();
    let initial_manifest =
        crate::managed_install::package_root(home).join("current/codex-package.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(initial_manifest).unwrap()).unwrap();
    let mut version = manifest["version"].as_str().unwrap().to_owned();
    tokio::spawn(async move {
        loop {
            let connection = listener.accept().await.expect("control connection");
            let mut websocket = tokio_tungstenite::accept_async(connection)
                .await
                .expect("websocket handshake");
            websocket
                .next()
                .await
                .expect("initialize request")
                .expect("frame");
            let path = crate::managed_install::package_root(&codex_home)
                .join("current/codex-package.json");
            // 선택 링크가 잠시 없어져도 이미 실행 중인 가짜 서버는 마지막 버전을 응답한다.
            match std::fs::read(path) {
                Ok(bytes) => {
                    let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    version = manifest["version"].as_str().unwrap().to_owned();
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("cannot read mock daemon manifest: {error}"),
            }
            websocket.send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({"id": 1, "result": {
                    "userAgent": format!("codex_app_server_daemon/{version}"),
                    "codexHome": codex_home, "platformFamily": "unix", "platformOs": std::env::consts::OS,
                }}).to_string().into(),
            )).await.expect("initialize response");
            websocket
                .next()
                .await
                .expect("initialized notification")
                .expect("frame");
        }
    })
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_start_and_restart_preserve_launch_features() {
    for features in [
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::from([
            ("api_key_model_discovery".to_string(), true),
            ("code_mode_host".to_string(), false),
        ]),
    ] {
        let home = tempfile::Builder::new()
            .prefix("cd-")
            .tempdir_in("/tmp")
            .unwrap();
        let (daemon, _) = manual_update_daemon(&home);
        let args_path = home.path().join("launch-args");
        std::fs::write(
        &daemon.settings_file,
        r#"{"featureOverrides":{"auth_elicitation":true},"updater":{"autoUpdateEnabled":false}}"#,
    )
    .unwrap();
        std::fs::write(&daemon.managed_codex_bin, format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo codex 1.0.0; exit; fi\nif [ \"$3\" = --help ]; then exit; fi\nprintf '%s\\n' \"$@\" > '{}'\nexec sleep 30\n",
        args_path.display(),
    )).unwrap();
        let control = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(/*secs*/ 10);
            while !args_path.exists() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "daemon did not launch"
                );
                tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
            }
            test_control_server(&daemon, home.path())
                .await
                .abort_handle()
        };
        let (started, server) = tokio::join!(daemon.start(&features), control);
        assert_eq!(started.unwrap().status, crate::LifecycleStatus::Started);
        assert_eq!(
            daemon.load_settings().await.unwrap().feature_overrides,
            features
        );
        let expected = if features.is_empty() {
            "app-server\n--listen\nunix://\n--managed-daemon\n"
        } else {
            "app-server\n--listen\nunix://\n-c\nfeatures.api_key_model_discovery=true\n-c\nfeatures.code_mode_host=false\n--managed-daemon\n"
        };
        assert_eq!(std::fs::read_to_string(&args_path).unwrap(), expected);
        let reused = daemon
            .start(&std::collections::BTreeMap::from([(
                "api_key_model_discovery".to_string(),
                false,
            )]))
            .await
            .unwrap();
        assert_eq!(reused.status, crate::LifecycleStatus::AlreadyRunning);
        assert_eq!(
            daemon.load_settings().await.unwrap().feature_overrides,
            features
        );
        assert_eq!(std::fs::read_to_string(&args_path).unwrap(), expected);
        std::fs::remove_file(&args_path).unwrap();
        let restarted = daemon.restart().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(/*secs*/ 10);
        while std::fs::read_to_string(&args_path).ok().as_deref() != Some(expected)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
        let args = std::fs::read_to_string(args_path);
        daemon.stop().await.unwrap();
        server.abort();
        assert_eq!(restarted.unwrap().status, crate::LifecycleStatus::Restarted);
        assert_eq!(args.unwrap(), expected);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn confirmed_feature_restart_preserves_ownership_and_skips_matching_settings() {
    use crate::LifecycleStatus;
    use std::collections::BTreeMap;

    for managed in [true, false] {
        let home = tempfile::Builder::new()
            .prefix("cd-")
            .tempdir_in("/tmp")
            .unwrap();
        let (daemon, _) = manual_update_daemon(&home);
        std::fs::write(&daemon.settings_file,
            r#"{"featureOverrides":{"auth_elicitation":true,"api_key_model_discovery":true},"updater":{"autoUpdateEnabled":false},"shutdownGraceSeconds":0}"#
        ).unwrap();
        let original = daemon.load_settings().await.unwrap();
        if managed {
            daemon.start_managed_backend(&original).await.unwrap();
        }
        let server = test_control_server(&daemon, home.path()).await;
        let _lock = daemon.acquire_operation_lock().await.unwrap();
        let requested = BTreeMap::from([
            ("api_key_model_discovery".to_string(), false),
            ("mcp_oauth_refresh_coordination".to_string(), true),
        ]);
        if managed {
            // 선택 링크만 숨겨 이미 시작된 shell이 실제 release의 스크립트를 계속 읽게 한다.
            let selected_package =
                crate::managed_install::package_root(home.path()).join("current");
            let saved_package = selected_package.with_extension("saved");
            std::fs::rename(&selected_package, &saved_package).unwrap();
            let error = daemon
                .restart_with_features_locked(&requested)
                .await
                .unwrap_err();
            std::fs::rename(saved_package, &selected_package).unwrap();
            assert!(
                error.to_string().contains("daemon executable not found"),
                "{error:#}"
            );
            assert_eq!(daemon.load_settings().await.unwrap(), original);
        }
        let result = daemon.restart_with_features_locked(&requested).await;
        if managed {
            assert_eq!(result.unwrap().status, LifecycleStatus::Restarted);
            let pid = std::fs::read(&daemon.pid_file).unwrap();
            let mut expected = original;
            expected.feature_overrides.extend(requested.clone());
            assert_eq!(daemon.load_settings().await.unwrap(), expected);
            assert_eq!(
                daemon
                    .restart_with_features_locked(&requested)
                    .await
                    .unwrap()
                    .status,
                LifecycleStatus::AlreadyRunning
            );
            assert_eq!(std::fs::read(&daemon.pid_file).unwrap(), pid);
            daemon.stop().await.unwrap();
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("no running managed daemon")
            );
            assert_eq!(daemon.load_settings().await.unwrap(), original);
        }
        server.abort();
    }
}

#[tokio::test]
async fn manual_update_restarts_with_auto_updates_disabled_and_helper_only_change() {
    for helper_only in [false, true] {
        let home = tempfile::Builder::new()
            .prefix("cd-")
            .tempdir_in("/tmp")
            .unwrap();
        let (daemon, _) = manual_update_daemon(&home);
        std::fs::write(
            &daemon.settings_file,
            r#"{"updater":{"autoUpdateEnabled":false},"shutdownGraceSeconds":0}"#,
        )
        .unwrap();
        let server = test_control_server(&daemon, home.path()).await;
        let settings = daemon.load_settings().await.unwrap();
        let backend = crate::backend::pid_backend(daemon.backend_paths(&settings));
        backend.start().await.unwrap();
        let current_pid = || {
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&daemon.pid_file).unwrap())
                .unwrap()["pid"]
                .as_u64()
                .unwrap()
        };
        let before = current_pid();
        let source = home.path().join("npm-source");
        let version = if helper_only { "1.0.0" } else { "1.1.0" };
        crate::prepare_install::tests::package(&source, version);
        std::fs::write(source.join("codex-resources/nested/runtime"), b"new helper").unwrap();
        let output = super::manual_update::run(
            &daemon,
            &executable_identity_from_reader(&b"updater"[..]).unwrap(),
            &mut test_terminate(),
            super::UpdateTrigger::Manual,
        )
        .await
        .unwrap();
        assert_eq!(output.status, UpdateStatus::Updated);
        assert_eq!(output.installed_version.as_deref(), Some(version));
        assert_eq!(output.running_version.as_deref(), Some(version));
        assert_ne!(current_pid(), before);
        let restarted = current_pid();
        let output = super::manual_update::run(
            &daemon,
            &executable_identity_from_reader(&b"updater"[..]).unwrap(),
            &mut test_terminate(),
            super::UpdateTrigger::Manual,
        )
        .await
        .unwrap();
        assert_eq!(output.status, UpdateStatus::NoUpdate);
        assert_eq!(current_pid(), restarted);
        backend.stop().await.unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn scheduled_sync_honors_auto_update_disabled() {
    let home = tempfile::Builder::new()
        .prefix("cd-")
        .tempdir_in("/tmp")
        .unwrap();
    let (daemon, _) = manual_update_daemon(&home);
    std::fs::write(
        &daemon.settings_file,
        r#"{"updater":{"autoUpdateEnabled":false}}"#,
    )
    .unwrap();
    let current = crate::managed_install::package_root(home.path())
        .join("current")
        .canonicalize()
        .unwrap();
    crate::prepare_install::tests::package(&home.path().join("npm-source"), "1.1.0");
    let result = super::update_once(
        &daemon,
        &executable_identity_from_reader(&b"updater"[..]).unwrap(),
        &mut test_terminate(),
        super::UpdateTrigger::Scheduled,
    )
    .await
    .unwrap();
    assert!(matches!(result, (super::UpdateLoopControl::Stop, None)));
    assert_eq!(
        crate::managed_install::package_root(home.path())
            .join("current")
            .canonicalize()
            .unwrap(),
        current
    );
}

#[tokio::test]
async fn missing_source_never_falls_back_to_public_installer() {
    let home = tempfile::Builder::new()
        .prefix("cd-")
        .tempdir_in("/tmp")
        .unwrap();
    let (daemon, _) = manual_update_daemon(&home);
    let current = crate::managed_install::package_root(home.path())
        .join("current")
        .canonicalize()
        .unwrap();
    std::fs::remove_dir_all(home.path().join("npm-source")).unwrap();
    let result = super::update_once(
        &daemon,
        &executable_identity_from_reader(&b"updater"[..]).unwrap(),
        &mut test_terminate(),
        super::UpdateTrigger::Manual,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        crate::managed_install::package_root(home.path())
            .join("current")
            .canonicalize()
            .unwrap(),
        current
    );
}
