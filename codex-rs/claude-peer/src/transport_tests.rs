#![allow(clippy::unwrap_used)]
use super::*;
use crate::protocol::user_frame;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use uuid::Uuid;

async fn start_peer() -> (tempfile::TempDir, std::sync::Arc<Peer>) {
    let temp = tempfile::Builder::new()
        .prefix("cp-")
        .tempdir_in("/tmp")
        .unwrap();
    let peer = Peer::start(PeerOptions {
        claude_home: temp.path().join("claude"),
        socket_directory: temp.path().join("socks"),
        inbox_path: temp.path().join("state/inbox.sqlite"),
        session_id: Uuid::new_v4().to_string(),
        name: "test-peer".into(),
        cwd: temp.path().display().to_string(),
        mode: PermissionMode::Prompting,
        policy: InboundPolicy::Parity,
    })
    .await
    .unwrap();
    (temp, peer)
}

async fn deliver(peer: &Peer, token: &str, message: Value) {
    let mut stream = UnixStream::connect(&peer.identity().await.messaging_socket_path)
        .await
        .unwrap();
    let mut bytes = serde_json::to_vec(&json!({"type":"auth","token":token})).unwrap();
    bytes.push(b'\n');
    bytes.extend(serde_json::to_vec(&message).unwrap());
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
    stream.shutdown().await.unwrap();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut bytes))
        .await
        .unwrap();
}

async fn frame(peer: &Peer, body: &str) -> Value {
    let identity = peer.identity().await;
    user_frame(
        &Uuid::new_v4().to_string(),
        &identity.address(),
        &identity.name,
        &identity.session_id,
        PermissionMode::Prompting,
        body,
        &[],
    )
    .unwrap()
}

#[tokio::test]
async fn authenticated_socket_input_is_durable_and_cleanup_is_owned() {
    let (_temp, peer) = start_peer().await;
    let identity = peer.identity().await;
    let record = peer.root.join(format!("{}.json", identity.pid));
    let key = crate::registry::key_path(&peer.root, &identity);
    assert_eq!(std::fs::metadata(&key).unwrap().mode() & 0o777, 0o600);
    let message = frame(&peer, "request one").await;
    deliver(&peer, &peer.token, message.clone()).await;
    let (_, received) = peer.pending().await.unwrap().unwrap();
    assert_eq!(received.body, "request one");
    assert_eq!(received.sender_pid, std::process::id());
    deliver(&peer, &peer.token, message).await;
    let (seq, received_again) = peer.pending().await.unwrap().unwrap();
    assert_eq!(received_again, received);
    peer.delivered(seq, &received).await.unwrap();
    assert_eq!(peer.pending().await.unwrap(), None);
    peer.shutdown().await;
    assert!(!record.exists());
    assert!(!key.exists());
    assert!(!identity.messaging_socket_path.exists());
}

#[tokio::test]
async fn invalid_auth_and_forged_sender_cannot_enqueue() {
    let (_temp, peer) = start_peer().await;
    deliver(
        &peer,
        "00000000000000000000000000000000",
        frame(&peer, "wrong token").await,
    )
    .await;
    assert_eq!(peer.pending().await.unwrap(), None);
    let mut forged = frame(&peer, "forged identity").await;
    forged["from"] = json!("uds:/tmp/cc-socks/999999.sock");
    deliver(&peer, &peer.token, forged).await;
    assert_eq!(peer.pending().await.unwrap(), None);
    peer.shutdown().await;
}

#[tokio::test]
async fn socket_permission_mismatch_is_held_until_human_decision() {
    let (_temp, peer) = start_peer().await;
    let identity = peer.identity().await;
    let frame = user_frame(
        &Uuid::new_v4().to_string(),
        &identity.address(),
        &identity.name,
        &identity.session_id,
        PermissionMode::Bypass,
        "hold this message",
        &[],
    )
    .unwrap();
    deliver(&peer, &peer.token, frame).await;
    assert_eq!(peer.pending().await.unwrap(), None);
    let held = peer.held_messages().await.unwrap();
    assert_eq!(held.len(), 1);
    peer.decide(held[0].0, true).await.unwrap();
    assert_eq!(
        peer.pending().await.unwrap().unwrap().1.body,
        "hold this message"
    );
    peer.shutdown().await;
}

#[tokio::test]
async fn symlink_key_is_rejected_before_sending() {
    let (temp, peer) = start_peer().await;
    let identity = peer.identity().await;
    let key = crate::registry::key_path(&peer.root, &identity);
    let relocated = temp.path().join("original.key");
    std::fs::rename(&key, &relocated).unwrap();
    std::os::unix::fs::symlink(&relocated, &key).unwrap();
    assert!(
        crate::transport::send(&peer.root, &identity, &frame(&peer, "blocked").await)
            .await
            .is_err()
    );
    peer.shutdown().await;
}

#[tokio::test]
async fn permission_change_rechecks_pending_and_requires_approval() {
    let (_temp, peer) = start_peer().await;
    deliver(
        &peer,
        &peer.token,
        frame(&peer, "pending under prompting").await,
    )
    .await;
    let (seq, message) = peer.pending().await.unwrap().unwrap();
    assert!(
        !peer
            .revalidate(seq, &message, PermissionMode::Bypass)
            .await
            .unwrap()
    );
    assert_eq!(peer.pending().await.unwrap(), None);
    assert_eq!(peer.held_messages().await.unwrap()[0].1.body, message.body);
    peer.shutdown().await;
}

#[tokio::test]
async fn failed_replacement_preserves_existing_peer_and_leaks_no_socket() {
    let (temp, peer) = start_peer().await;
    let identity = peer.identity().await;
    let options = PeerOptions {
        claude_home: temp.path().join("claude"),
        socket_directory: temp.path().join("socks"),
        inbox_path: temp.path().join("state/inbox.sqlite"),
        session_id: Uuid::new_v4().to_string(),
        name: String::new(),
        cwd: temp.path().display().to_string(),
        mode: PermissionMode::Prompting,
        policy: InboundPolicy::Parity,
    };
    assert!(Peer::replace(options, &peer).await.is_err());
    assert!(!peer.is_closed());
    assert!(identity.messaging_socket_path.exists());
    deliver(&peer, &peer.token, frame(&peer, "still live").await).await;
    assert_eq!(peer.pending().await.unwrap().unwrap().1.body, "still live");
    peer.shutdown().await;
}

#[tokio::test]
async fn old_generation_cleanup_does_not_remove_replacement() {
    let (temp, previous) = start_peer().await;
    let replacement = Peer::replace(
        PeerOptions {
            claude_home: temp.path().join("claude"),
            socket_directory: temp.path().join("socks"),
            inbox_path: temp.path().join("state/new.sqlite"),
            session_id: Uuid::new_v4().to_string(),
            name: "next-peer".into(),
            cwd: temp.path().display().to_string(),
            mode: PermissionMode::Prompting,
            policy: InboundPolicy::Parity,
        },
        &previous,
    )
    .await
    .unwrap();
    previous.shutdown().await;
    let identity = replacement.identity().await;
    assert!(identity.messaging_socket_path.exists());
    assert_eq!(replacement.list_sessions().await.unwrap(), vec![identity]);
    deliver(
        &replacement,
        &replacement.token,
        frame(&replacement, "new generation").await,
    )
    .await;
    assert_eq!(
        replacement.pending().await.unwrap().unwrap().1.body,
        "new generation"
    );
    replacement.shutdown().await;
}

#[tokio::test]
async fn failed_activation_can_restore_previous_generation() {
    let (temp, previous) = start_peer().await;
    let identity = previous.identity().await;
    let replacement = Peer::replace(
        PeerOptions {
            claude_home: temp.path().join("claude"),
            socket_directory: temp.path().join("socks"),
            inbox_path: temp.path().join("other/inbox.sqlite"),
            session_id: Uuid::new_v4().to_string(),
            name: "replacement".into(),
            cwd: temp.path().display().to_string(),
            mode: PermissionMode::Prompting,
            policy: InboundPolicy::Parity,
        },
        &previous,
    )
    .await
    .unwrap();
    replacement.shutdown().await;
    previous.restore_registration().await.unwrap();
    assert_eq!(previous.list_sessions().await.unwrap(), vec![identity]);
    deliver(
        &previous,
        &previous.token,
        frame(&previous, "restored").await,
    )
    .await;
    assert_eq!(
        previous.pending().await.unwrap().unwrap().1.body,
        "restored"
    );
    previous.shutdown().await;
}

#[tokio::test]
async fn stale_reused_pid_record_does_not_block_new_registration() {
    let (temp, previous) = start_peer().await;
    let mut identity = previous.identity().await;
    previous.shutdown().await;
    identity.proc_start = "previous process start".into();
    let root = temp.path().join("claude/sessions");
    crate::registry::write_private_json(&root.join(format!("{}.json", identity.pid)), &identity)
        .unwrap();
    let peer = Peer::start(PeerOptions {
        claude_home: temp.path().join("claude"),
        socket_directory: temp.path().join("socks"),
        inbox_path: temp.path().join("state/inbox.sqlite"),
        session_id: Uuid::new_v4().to_string(),
        name: "reused-pid".into(),
        cwd: temp.path().display().to_string(),
        mode: PermissionMode::Prompting,
        policy: InboundPolicy::Parity,
    })
    .await
    .unwrap();
    assert_ne!(peer.identity().await.proc_start, identity.proc_start);
    peer.shutdown().await;
}
