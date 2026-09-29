//! Claude Code 로컬 peer 프로토콜. Core 및 Team inbox와 독립된 전송 계층이다.

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

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod protocol_tests;

#[cfg(all(test, unix))]
#[path = "transport_tests.rs"]
mod transport_tests;
