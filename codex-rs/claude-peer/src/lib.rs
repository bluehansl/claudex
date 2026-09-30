//! Claude Code 로컬 peer 프로토콜. Core 및 Team inbox와 독립된 전송 계층이다.

pub mod names;
mod protocol;
#[cfg(unix)]
mod receive;
#[cfg(unix)]
mod registry;
#[cfg(unix)]
mod service;
#[cfg(unix)]
mod store;
#[cfg(unix)]
mod transport;

pub use protocol::InboundPolicy;
pub use protocol::PermissionMode;
pub use protocol::ReceivedMessage;
#[cfg(unix)]
pub use registry::PeerIdentity;
#[cfg(unix)]
pub use service::Peer;
#[cfg(unix)]
pub use service::PeerOptions;
#[cfg(unix)]
pub use service::PeerOwnership;
#[cfg(unix)]
pub use service::shutdown_owned_peers;

#[cfg(unix)]
pub async fn list_sessions(claude_home: &std::path::Path) -> anyhow::Result<Vec<PeerIdentity>> {
    registry::list(&claude_home.join("sessions")).await
}

#[cfg(unix)]
pub async fn pending_approvals(
    database: &std::path::Path,
) -> anyhow::Result<Vec<(i64, ReceivedMessage)>> {
    anyhow::ensure!(database.exists(), "peer inbox does not exist");
    store::Inbox::open(database).await?.held().await
}

#[cfg(unix)]
pub async fn decide_approval(
    database: &std::path::Path,
    seq: i64,
    approve: bool,
) -> anyhow::Result<bool> {
    anyhow::ensure!(database.exists(), "peer inbox does not exist");
    Ok(store::Inbox::open(database)
        .await?
        .decide(seq, approve)
        .await?
        .is_some())
}

/// TUI와 daemon이 공유하는 영속 inbox의 기록 확인. 소켓 소유권은 TUI에만 있다.
#[cfg(unix)]
pub async fn confirm_external_recorded(
    database: &std::path::Path,
    message: &ReceivedMessage,
) -> anyhow::Result<bool> {
    if !database.exists() {
        return Ok(false);
    }
    let inbox = store::Inbox::open(database).await?;
    for (seq, stored) in inbox.processing().await? {
        if stored == *message {
            inbox.mark_recorded(seq).await?;
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(unix)]
pub async fn external_processing(
    database: &std::path::Path,
) -> anyhow::Result<Vec<(i64, ReceivedMessage)>> {
    if !database.exists() {
        return Ok(Vec::new());
    }
    store::Inbox::open(database).await?.processing().await
}

#[cfg(unix)]
pub async fn first_external_attempt(database: &std::path::Path, seq: i64) -> anyhow::Result<bool> {
    anyhow::ensure!(database.exists(), "peer inbox does not exist");
    store::Inbox::open(database).await?.first_attempt(seq).await
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod protocol_tests;

#[cfg(all(test, unix))]
#[path = "transport_tests.rs"]
mod transport_tests;
