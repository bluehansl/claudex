//! `PeerServer` 단위 테스트: 소유권 예약, 확정 rename 반영, 백엔드/홈 거절, 폐기 후 처리 중단.
//!
//! production 모듈이 `#[cfg(test)] #[path = "claude_peer_tests.rs"] mod ...;` 로 연결한다.
//! registry 는 임시 `claude_config_dir` 에만 쓰고, 소켓은 임시 디렉터리(또는 production 이
//! 고정한 `/tmp/cc-socks`)에 만든 뒤 shutdown 으로 정리한다. 공유 daemon/MCP 왕복은 다루지 않는다.

use super::*;
use crate::legacy_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::unbounded_channel;

struct Fixture {
    _codex_home: tempfile::TempDir,
    _claude_home: tempfile::TempDir,
    socket_dir: tempfile::TempDir,
    config: Config,
}

async fn fixture() -> Fixture {
    let codex_home = tempfile::Builder::new()
        .prefix("claudex-peer-home-")
        .tempdir()
        .expect("codex home tempdir");
    let claude_home = tempfile::Builder::new()
        .prefix("claude-config-")
        .tempdir()
        .expect("claude home tempdir");
    // Unix 소켓 경로 길이 제한(104바이트)을 피하려고 /tmp 바로 아래에 만든다.
    let socket_dir = tempfile::Builder::new()
        .prefix("cc-socks-test-")
        .tempdir_in("/tmp")
        .expect("socket tempdir");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("config");
    config.claude_config_dir = claude_home.path().to_path_buf();
    Fixture {
        _codex_home: codex_home,
        _claude_home: claude_home,
        socket_dir,
        config,
    }
}

fn inbox_path(config: &Config, thread_id: ThreadId) -> PathBuf {
    config
        .codex_home
        .join("claude-peer")
        .join(format!("{thread_id}.sqlite"))
        .to_path_buf()
}

fn sessions_dir(config: &Config) -> PathBuf {
    config.claude_config_dir.join("sessions")
}

fn registry_record(config: &Config) -> Option<Value> {
    let path = sessions_dir(config).join(format!("{}.json", std::process::id()));
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn key_files(config: &Config) -> usize {
    std::fs::read_dir(sessions_dir(config))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".key"))
                .count()
        })
        .unwrap_or(0)
}

/// HTTP MCP 서버와 monitor 없이 `PeerServer` 를 조립한다. `connection` 이 없으므로
/// `step()` 은 등록/상태 갱신까지만 진행하고 daemon 전달은 하지 않는다.
fn frontend(config: Config) -> (Arc<PeerServer>, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = unbounded_channel::<AppEvent>();
    let controller = Arc::new(Controller {
        config,
        context: watch::channel(None).0,
        connection: RwLock::new(None),
        peer: Mutex::new(None),
        cancel: CancellationToken::new(),
        events: AppEventSender::new(tx),
        reservation: std::sync::Mutex::new(None),
        observer_thread: std::sync::Mutex::new(None),
    });
    let server = Arc::new(PeerServer {
        config: json!({
            "url": "http://127.0.0.1:1/mcp",
            "http_headers": {"Authorization": "Bearer test-token"},
            "default_tools_approval_mode": "approve"
        }),
        controller,
        http: tokio::spawn(async {}),
        monitor: Mutex::new(None),
    });
    (server, rx)
}

fn context(thread_id: ThreadId, name: &str) -> PeerContext {
    PeerContext {
        thread_id,
        name: name.to_owned(),
        cwd: PathBuf::from("/tmp/project"),
        mode: PermissionMode::Prompting,
        busy: false,
        can_receive: true,
    }
}

async fn start_peer(fixture: &Fixture, thread_id: ThreadId, name: &str) -> Arc<Peer> {
    Peer::start(PeerOptions {
        claude_home: fixture.config.claude_config_dir.clone(),
        socket_directory: fixture.socket_dir.path().join("socks"),
        inbox_path: inbox_path(&fixture.config, thread_id),
        session_id: thread_id.to_string(),
        name: name.to_owned(),
        cwd: "/tmp/project".to_owned(),
        mode: PermissionMode::Prompting,
        policy: InboundPolicy::Parity,
    })
    .await
    .expect("peer starts in temporary directories")
}

fn notices(rx: &mut UnboundedReceiver<AppEvent>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::PeerNotice(message) = event {
            out.push(message);
        }
    }
    out
}

fn ticks(rx: &mut UnboundedReceiver<AppEvent>) -> usize {
    let mut count = 0;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AppEvent::PeerTick) {
            count += 1;
        }
    }
    count
}

fn reserved_thread(server: &PeerServer) -> Option<String> {
    server
        .controller
        .reservation
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|(thread_id, _)| thread_id.to_string())
}

fn release_reservation(server: &PeerServer) {
    server
        .controller
        .reservation
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
}

async fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..60 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    condition()
}

// ---------------------------------------------------------------------------
// (3) supported_backend / matching_home 거절
// ---------------------------------------------------------------------------

#[test]
fn supported_backend_rejects_versions_before_0_159_1_unless_embedded() {
    for rejected in [
        Some("0.158.9"),
        Some("0.159.0"),
        Some("0.159.1-alpha.1"),
        Some("v0.159.1"),
        Some(" 0.159.1"),
        Some("unknown"),
        Some(""),
        None,
    ] {
        assert!(!supported_backend(rejected, false), "{rejected:?}");
    }
    for accepted in [
        Some("0.159.1"),
        Some("0.159.2"),
        Some("0.160.0"),
        Some("1.0.0"),
    ] {
        assert!(supported_backend(accepted, false), "{accepted:?}");
    }
    // 내장 app-server 는 버전 협상 없이 허용한다.
    assert!(supported_backend(None, true));
    assert!(supported_backend(Some("0.1.0"), true));
    assert!(supported_backend(Some("unknown"), true));
}

#[test]
fn matching_home_accepts_only_the_same_canonical_directory() {
    let home = tempfile::tempdir().expect("home");
    let other = tempfile::tempdir().expect("other");
    let home_str = home.path().to_string_lossy().into_owned();
    let other_str = other.path().to_string_lossy().into_owned();

    assert!(matching_home(home.path(), Some(home_str.as_str()), false));
    assert!(matching_home(
        home.path(),
        Some(format!("{home_str}/").as_str()),
        false
    ));

    let url = url::Url::from_file_path(home.path())
        .expect("file url")
        .to_string();
    assert!(url.starts_with("file:"));
    assert!(matching_home(home.path(), Some(url.as_str()), false));

    assert!(!matching_home(home.path(), Some(other_str.as_str()), false));
    assert!(!matching_home(home.path(), None, false));
    assert!(!matching_home(
        home.path(),
        Some(format!("{home_str}/missing").as_str()),
        false
    ));
    assert!(!matching_home(
        home.path(),
        Some("file:///nonexistent-claudex-home"),
        false
    ));
    assert!(!matching_home(
        &home.path().join("missing"),
        Some(home_str.as_str()),
        false
    ));

    // 심볼릭 링크는 canonical 경로로 비교한다.
    let link = other.path().join("home-link");
    std::os::unix::fs::symlink(home.path(), &link).expect("symlink");
    let link_str = link.to_string_lossy().into_owned();
    assert!(matching_home(&link, Some(home_str.as_str()), false));
    assert!(matching_home(home.path(), Some(link_str.as_str()), false));

    // 내장 app-server 는 서버 홈과 무관하게 허용한다.
    assert!(matching_home(home.path(), None, true));
    assert!(matching_home(home.path(), Some(other_str.as_str()), true));
}

#[test]
fn fallback_name_uses_the_last_eight_characters_of_the_thread_id() {
    let thread_id = ThreadId::new();
    let id = thread_id.to_string();
    assert_eq!(
        fallback_name(thread_id),
        format!("claudex-{}", &id[id.len() - 8..])
    );
    assert!(codex_claude_peer::names::validate_name(&fallback_name(thread_id)).is_ok());
}

#[test]
fn retry_state_starts_ready_without_a_message() {
    let retry = RetryState::default();
    assert!(retry.next <= Instant::now());
    assert!(retry.message.is_none());
    assert_eq!(retry.attempts, 0);
}

#[test]
fn tool_arguments_reject_unknown_fields() {
    let args: SendArgs = serde_json::from_value(json!({
        "target": "uds:/tmp/cc-socks/1.sock",
        "message": "hello"
    }))
    .expect("send args");
    assert_eq!(args.target, "uds:/tmp/cc-socks/1.sock");
    assert_eq!(args.message, "hello");
    assert!(!args.notify_when_idle);

    let args: SendArgs = serde_json::from_value(json!({
        "target": "peer",
        "message": "hello",
        "notify_when_idle": true
    }))
    .expect("send args with subscription");
    assert!(args.notify_when_idle);

    assert!(serde_json::from_value::<SendArgs>(json!({"to": "peer", "message": "hello"})).is_err());
    assert!(
        serde_json::from_value::<SendArgs>(
            json!({"target": "peer", "message": "hello", "extra": 1})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<SendArgs>(json!({"target": "peer"})).is_err());

    let status: StatusArgs =
        serde_json::from_value(json!({"message_id": "abc"})).expect("status args");
    assert_eq!(status.message_id, "abc");
    assert!(
        serde_json::from_value::<StatusArgs>(json!({"message_id": "abc", "extra": true})).is_err()
    );
}

// ---------------------------------------------------------------------------
// (2) update / rename: 확정 rename 만 반영, thread/socket identity 유지
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_keeps_the_confirmed_name_for_the_same_thread_and_replaces_other_threads() {
    let fixture = fixture().await;
    let (server, _rx) = frontend(fixture.config.clone());
    let thread = ThreadId::new();

    server.update(Some(context(thread, "alpha")));
    let current = server.controller.context().expect("context");
    assert_eq!(current.name, "alpha");

    // 화면의 낙관적 rename 은 무시하되 다른 필드 변화는 반영한다.
    let mut optimistic = context(thread, "optimistic");
    optimistic.busy = true;
    optimistic.mode = PermissionMode::Bypass;
    server.update(Some(optimistic));
    let current = server.controller.context().expect("context");
    assert_eq!(current.name, "alpha");
    assert!(current.busy);
    assert!(matches!(current.mode, PermissionMode::Bypass));
    assert_eq!(current.thread_id.to_string(), thread.to_string());

    // 이름만 다른 동일 상태는 변경으로 통지되지 않는다.
    let mut watcher = server.controller.context.subscribe();
    {
        let _seen = watcher.borrow_and_update();
    }
    let mut same = context(thread, "still-optimistic");
    same.busy = true;
    same.mode = PermissionMode::Bypass;
    server.update(Some(same));
    assert!(!watcher.has_changed().expect("watch open"));

    // 다른 스레드는 이름까지 통째로 바뀐다.
    let other = ThreadId::new();
    server.update(Some(context(other, "beta")));
    let current = server.controller.context().expect("context");
    assert_eq!(current.name, "beta");
    assert_eq!(current.thread_id.to_string(), other.to_string());
    assert!(!current.busy);
    assert!(watcher.has_changed().expect("watch open"));

    server.update(None);
    assert!(server.controller.context().is_none());
}

#[tokio::test]
async fn rename_applies_only_confirmed_names_to_the_active_thread() {
    let fixture = fixture().await;
    let (server, _rx) = frontend(fixture.config.clone());
    let thread = ThreadId::new();
    let other = ThreadId::new();

    // 활성 대화가 없으면 rename 은 아무것도 만들지 않는다.
    server.rename(thread, Some("ignored"));
    assert!(server.controller.context().is_none());

    server.update(Some(context(thread, "alpha")));
    server.rename(other, Some("wrong-thread"));
    assert_eq!(server.controller.context().expect("context").name, "alpha");

    server.rename(thread, Some("claudex 업데이트"));
    let current = server.controller.context().expect("context");
    assert_eq!(current.name, "claudex 업데이트");
    assert_eq!(current.thread_id.to_string(), thread.to_string());
    assert_eq!(current.cwd, PathBuf::from("/tmp/project"));
    assert!(!current.busy);
    assert!(current.can_receive);
    assert!(matches!(current.mode, PermissionMode::Prompting));

    // 빈 이름·None 은 fallback 으로 확정된다.
    server.rename(thread, Some("   "));
    assert_eq!(
        server.controller.context().expect("context").name,
        fallback_name(thread)
    );
    server.rename(thread, Some("named-again"));
    assert_eq!(
        server.controller.context().expect("context").name,
        "named-again"
    );
    server.rename(thread, None);
    assert_eq!(
        server.controller.context().expect("context").name,
        fallback_name(thread)
    );

    // 이후 낙관적 update 도 확정된 fallback 을 덮지 못한다.
    server.update(Some(context(thread, "optimistic")));
    assert_eq!(
        server.controller.context().expect("context").name,
        fallback_name(thread)
    );
}

#[tokio::test]
async fn status_and_permission_updates_target_only_the_active_thread() {
    let fixture = fixture().await;
    let (server, _rx) = frontend(fixture.config.clone());
    let thread = ThreadId::new();
    let other = ThreadId::new();

    server.update_status(thread, true);
    server.update_permissions(thread, PermissionMode::Bypass);
    server.close_thread(thread);
    assert!(server.controller.context().is_none());

    server.update(Some(context(thread, "alpha")));
    server.update_status(other, true);
    server.update_permissions(other, PermissionMode::Bypass);
    let current = server.controller.context().expect("context");
    assert!(!current.busy);
    assert!(matches!(current.mode, PermissionMode::Prompting));

    server.update_status(thread, true);
    server.update_permissions(thread, PermissionMode::Bypass);
    let current = server.controller.context().expect("context");
    assert!(current.busy);
    assert!(matches!(current.mode, PermissionMode::Bypass));
    assert_eq!(current.name, "alpha");
    assert_eq!(current.thread_id.to_string(), thread.to_string());

    server.close_thread(other);
    assert!(server.controller.context().is_some());
    server.close_thread(thread);
    assert!(server.controller.context().is_none());
}

#[tokio::test]
async fn peer_rename_keeps_its_socket_reference_and_session() {
    let fixture = fixture().await;
    let thread = ThreadId::new();
    let peer = start_peer(&fixture, thread, "before-rename").await;
    let before = peer.identity().await;
    assert_eq!(before.name, "before-rename");
    assert_eq!(before.session_id, thread.to_string());
    assert!(
        before
            .messaging_socket_path
            .starts_with(fixture.socket_dir.path())
    );

    peer.rename("after-rename").await.expect("rename");
    let after = peer.identity().await;
    assert_eq!(after.name, "after-rename");
    assert_eq!(after.name_source, "user");
    assert_eq!(after.messaging_socket_path, before.messaging_socket_path);
    assert_eq!(after.reference(), before.reference());
    assert_eq!(after.address(), before.address());
    assert_eq!(after.session_id, before.session_id);
    assert_eq!(after.pid, before.pid);
    assert!(after.name_since >= before.name_since);

    let record = registry_record(&fixture.config).expect("registry record");
    assert_eq!(record["name"], "after-rename");
    assert_eq!(
        record["messagingSocketPath"],
        before.messaging_socket_path.to_string_lossy().as_ref()
    );
    assert_eq!(record["sessionId"], thread.to_string());

    // 무효 이름은 거부되고 identity 는 그대로다. 같은 이름은 no-op 이다.
    assert!(peer.rename("bad\"name").await.is_err());
    assert_eq!(peer.identity().await.name, "after-rename");
    assert!(peer.rename("after-rename").await.is_ok());
    assert_eq!(
        registry_record(&fixture.config).expect("registry record")["name"],
        "after-rename"
    );

    peer.shutdown().await;
    assert!(peer.is_closed());
    assert!(registry_record(&fixture.config).is_none());
    assert_eq!(key_files(&fixture.config), 0);
    assert!(!before.messaging_socket_path.exists());
}

// ---------------------------------------------------------------------------
// (1) reserve_attachment: 소유권은 대화당 하나, 실패한 frontend 는 config 를 덮지 않는다
// ---------------------------------------------------------------------------

/// 두 번째 frontend 는 `owns == false` 를 받는다. 호출자는 이 값이 false 이면 `configure()` 를
/// 호출하지 않아야 하며, 그 게이트는 startup/thread routing 쪽 production 코드가 책임진다.
#[tokio::test]
async fn reserve_attachment_grants_ownership_to_one_frontend_and_notifies_the_other() {
    let fixture = fixture().await;
    let (first, mut first_rx) = frontend(fixture.config.clone());
    let (second, mut second_rx) = frontend(fixture.config.clone());
    let thread = ThreadId::new();

    let attachment = first.reserve_attachment(thread).await;
    assert!(attachment.owns);
    assert!(attachment.owner.is_some());
    assert_eq!(attachment.thread_id.to_string(), thread.to_string());
    first.commit_attachment(attachment);
    assert_eq!(reserved_thread(&first), Some(thread.to_string()));
    assert!(notices(&mut first_rx).is_empty());

    let attachment = second.reserve_attachment(thread).await;
    assert!(!attachment.owns);
    assert!(attachment.owner.is_none());
    assert_eq!(attachment.thread_id.to_string(), thread.to_string());
    let second_notices = notices(&mut second_rx);
    assert_eq!(second_notices.len(), 1);
    assert!(second_notices[0].contains("Another terminal owns messaging"));
    second.commit_attachment(attachment);
    assert!(reserved_thread(&second).is_none());
    assert_eq!(reserved_thread(&first), Some(thread.to_string()));

    // 다른 대화는 독립적으로 소유할 수 있다.
    let other = ThreadId::new();
    let attachment = second.reserve_attachment(other).await;
    assert!(attachment.owns);
    second.commit_attachment(attachment);
    assert_eq!(reserved_thread(&second), Some(other.to_string()));
    assert!(notices(&mut second_rx).is_empty());

    // 첫 frontend 가 소유권을 놓으면 두 번째가 같은 대화를 이어받는다.
    release_reservation(&first);
    let attachment = second.reserve_attachment(thread).await;
    assert!(attachment.owns);
    assert!(attachment.owner.is_some());
}

#[tokio::test]
async fn committing_a_non_owning_attachment_never_replaces_a_reservation() {
    let fixture = fixture().await;
    let (server, _rx) = frontend(fixture.config.clone());
    let thread = ThreadId::new();

    let attachment = server.reserve_attachment(thread).await;
    assert!(attachment.owns);
    server.commit_attachment(attachment);

    server.commit_attachment(PeerAttachment {
        owns: false,
        thread_id: ThreadId::new(),
        owner: None,
    });
    assert_eq!(reserved_thread(&server), Some(thread.to_string()));
}

#[tokio::test]
async fn reserve_attachment_yields_to_a_conversation_owned_by_a_live_peer() {
    let fixture = fixture().await;
    let thread = ThreadId::new();
    // 다른 터미널이 이미 이 대화의 소켓과 inbox 를 소유한 상황.
    let other_terminal = start_peer(&fixture, thread, "other-terminal").await;
    let (server, mut rx) = frontend(fixture.config.clone());

    let attachment = server.reserve_attachment(thread).await;
    assert!(!attachment.owns);
    assert!(attachment.owner.is_none());
    assert!(
        notices(&mut rx)
            .iter()
            .any(|notice| notice.contains("Another terminal owns messaging"))
    );
    server.commit_attachment(attachment);
    assert!(reserved_thread(&server).is_none());

    other_terminal.shutdown().await;
    let attachment = server.reserve_attachment(thread).await;
    assert!(attachment.owns);
    assert!(attachment.owner.is_some());
}

#[tokio::test]
async fn reserve_attachment_reuses_the_frontends_own_live_peer() {
    let fixture = fixture().await;
    let thread = ThreadId::new();
    let (server, mut rx) = frontend(fixture.config.clone());
    let peer = start_peer(&fixture, thread, "own-peer").await;
    *server.controller.peer.lock().await = Some(Arc::clone(&peer));

    // 살아있는 자기 peer 가 같은 대화를 소유하면 새 lock 없이 소유로 본다.
    let attachment = server.reserve_attachment(thread).await;
    assert!(attachment.owns);
    assert!(attachment.owner.is_none());
    assert!(notices(&mut rx).is_empty());

    // 다른 대화는 여전히 자기 lock 이 필요하다.
    let other = ThreadId::new();
    let attachment = server.reserve_attachment(other).await;
    assert!(attachment.owns);
    assert!(attachment.owner.is_some());

    // 닫힌 peer 는 소유 근거가 되지 못하고 lock 을 새로 잡는다.
    peer.shutdown().await;
    assert!(peer.is_closed());
    let attachment = server.reserve_attachment(thread).await;
    assert!(attachment.owns);
    assert!(attachment.owner.is_some());
}

#[tokio::test]
async fn configure_publishes_the_cross_session_server_and_inbound_policy() {
    let fixture = fixture().await;
    let (server, _rx) = frontend(fixture.config.clone());

    let mut overrides = None;
    server.configure(&mut overrides);
    let values = overrides.expect("overrides");
    assert_eq!(
        values.get("mcp_servers.cross_session"),
        Some(&server.config)
    );
    assert_eq!(values.get("claude_peer_inbound"), Some(&json!("parity")));

    let mut again = Some(values.clone());
    server.configure(&mut again);
    assert_eq!(again.expect("overrides"), values);
}

#[tokio::test]
async fn resume_transport_does_not_overwrite_the_owning_frontends_configuration() {
    use crate::dynamic_tools_mcp::ThreadToolTransport;
    let fixture = fixture().await;
    let (owner, _) = frontend(fixture.config.clone());
    let (mut observer, _) = frontend(fixture.config.clone());
    Arc::get_mut(&mut observer).expect("unique frontend").config =
        json!({"url":"http://127.0.0.1:2/mcp"});
    let thread = ThreadId::new();
    owner.commit_attachment(owner.reserve_attachment(thread).await);
    let attachment = observer.reserve_attachment(thread).await;
    assert!(!attachment.owns);
    let transport = ThreadToolTransport::WithPeer(
        Box::new(ThreadToolTransport::Disabled),
        Arc::clone(&observer),
    );
    let mut config = Some(std::collections::HashMap::from([
        ("mcp_servers.cross_session".into(), owner.config.clone()),
        ("approval_policy".into(), json!("on-request")),
    ]));
    transport.configure_resume_mcp(&mut config, attachment.owns);
    assert_eq!(
        config.as_ref().unwrap().get("mcp_servers.cross_session"),
        Some(&owner.config)
    );
    release_reservation(&owner);
    let attachment = observer.reserve_attachment(thread).await;
    assert!(attachment.owns);
    transport.configure_resume_mcp(&mut config, attachment.owns);
    assert_eq!(
        config.as_ref().unwrap().get("mcp_servers.cross_session"),
        Some(&observer.config)
    );
    assert_eq!(
        config.as_ref().unwrap().get("approval_policy"),
        Some(&json!("on-request"))
    );
}

#[tokio::test]
async fn shutdown_frontend_cannot_reserve_new_conversations() {
    let fixture = fixture().await;
    let (server, _) = frontend(fixture.config.clone());
    server.shutdown().await;
    let attachment = server.reserve_attachment(ThreadId::new()).await;
    assert!(!attachment.owns);
    assert!(attachment.owner.is_none());
    assert!(reserved_thread(&server).is_none());
}

// ---------------------------------------------------------------------------
// (4) 폐기(shutdown)된 frontend 는 새 메시지를 처리하지 않는다
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_loop_registers_from_a_reservation_and_stops_after_shutdown() {
    let fixture = fixture().await;
    let thread = ThreadId::new();
    let (server, mut rx) = frontend(fixture.config.clone());

    let attachment = server.reserve_attachment(thread).await;
    assert!(attachment.owns);
    server.commit_attachment(attachment);
    server.update(Some(context(thread, "reserved-peer")));

    let monitor = tokio::spawn(Arc::clone(&server.controller).run());
    *server.monitor.lock().await = Some(monitor);

    let registered = wait_for(|| {
        registry_record(&fixture.config).is_some_and(|record| record["status"] == "idle")
    })
    .await;
    assert!(
        registered,
        "run loop should publish the registry record from the reservation"
    );
    let record = registry_record(&fixture.config).expect("record");
    assert_eq!(record["name"], "reserved-peer");
    assert_eq!(record["sessionId"], thread.to_string());
    assert_eq!(record["pid"], std::process::id());
    assert_eq!(record["peerProtocol"], 1);
    assert_eq!(record["peerFeatures"], json!(["notify_idle"]));
    let socket = PathBuf::from(record["messagingSocketPath"].as_str().expect("socket path"));
    assert!(socket.starts_with("/tmp/cc-socks"));
    assert!(socket.exists());
    assert!(
        reserved_thread(&server).is_none(),
        "registration consumes the reservation"
    );
    assert_eq!(key_files(&fixture.config), 1);
    assert!(ticks(&mut rx) >= 1);

    server.shutdown().await;
    assert!(server.controller.cancel.is_cancelled());
    assert!(server.controller.peer.lock().await.is_none());
    assert!(registry_record(&fixture.config).is_none());
    assert_eq!(key_files(&fixture.config), 0);
    assert!(!socket.exists());
    assert!(tokio::net::UnixStream::connect(&socket).await.is_err());

    // 루프가 죽었으므로 tick 도, 재등록도 없다.
    let _ = ticks(&mut rx);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(ticks(&mut rx), 0);
    server.update(Some(context(thread, "reserved-peer-again")));
    server.rename(thread, Some("renamed-after-dispose"));
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(ticks(&mut rx), 0);
    assert!(server.controller.peer.lock().await.is_none());
    assert!(registry_record(&fixture.config).is_none());
    assert!(reserved_thread(&server).is_none());
}

#[tokio::test]
async fn observer_does_not_claim_messaging_until_explicit_resume() {
    let fixture = fixture().await;
    let thread = ThreadId::new();
    let owner = start_peer(&fixture, thread, "owner").await;
    let (observer, _) = frontend(fixture.config.clone());
    let attachment = observer.reserve_attachment(thread).await;
    assert!(!attachment.owns);
    observer.commit_attachment(attachment);
    observer.update(Some(context(thread, "observer")));
    owner.shutdown().await;
    observer
        .controller
        .step(&mut RetryState::default())
        .await
        .unwrap();
    assert!(observer.controller.peer.lock().await.is_none());
    let attachment = observer.reserve_attachment(thread).await;
    assert!(attachment.owns);
    observer.commit_attachment(attachment);
    assert!(
        observer
            .controller
            .observer_thread
            .lock()
            .unwrap()
            .is_none()
    );
    release_reservation(&observer);
    observer.shutdown().await;
}

#[tokio::test]
async fn run_loop_drops_the_peer_when_the_conversation_closes() {
    let fixture = fixture().await;
    let thread = ThreadId::new();
    let (server, mut rx) = frontend(fixture.config.clone());
    let peer = start_peer(&fixture, thread, "closing-peer").await;
    let socket = peer.identity().await.messaging_socket_path;
    *server.controller.peer.lock().await = Some(Arc::clone(&peer));
    server.update(Some(context(thread, "closing-peer")));

    let monitor = tokio::spawn(Arc::clone(&server.controller).run());
    *server.monitor.lock().await = Some(monitor);
    assert!(
        wait_for(
            || registry_record(&fixture.config).is_some_and(|record| record["status"] == "idle")
        )
        .await
    );

    // 대화가 닫히면 다음 step 이 peer 를 닫고 등록을 지운다.
    server.close_thread(thread);
    assert!(wait_for(|| registry_record(&fixture.config).is_none()).await);
    assert!(wait_for(|| peer.is_closed()).await);
    assert!(!socket.exists());
    assert!(server.controller.peer.lock().await.is_none());

    server.shutdown().await;
    let _ = ticks(&mut rx);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(ticks(&mut rx), 0);
}
