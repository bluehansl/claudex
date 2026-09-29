use crate::protocol::MAX_FRAME_BYTES;
use crate::registry::PeerIdentity;
use crate::registry::PeerKey;
use crate::registry::key_path;
use crate::registry::read_owned_file;
use crate::registry::socket_metadata;
use crate::registry::uid;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::time::timeout;

pub(crate) fn peer_pid(stream: &UnixStream) -> Result<u32> {
    let credentials = stream.peer_cred()?;
    if credentials.uid() != uid() {
        bail!("peer socket belongs to another OS user");
    }
    credentials
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| anyhow::anyhow!("OS peer process identity is unavailable"))
}

pub(crate) async fn send(root: &Path, peer: &PeerIdentity, frame: &Value) -> Result<()> {
    let line = serde_json::to_vec(frame)?;
    if line.len() > MAX_FRAME_BYTES {
        bail!("peer frame is too large");
    }
    if !peer.is_live().await {
        bail!("peer process restarted or exited; refresh the session list");
    }
    socket_metadata(&peer.messaging_socket_path)?;
    let key_bytes = read_owned_file(&key_path(root, peer), 4096, true)
        .map_err(|_| anyhow::anyhow!("peer authentication key is unavailable"))?;
    let key: PeerKey = serde_json::from_slice(&key_bytes)
        .map_err(|_| anyhow::anyhow!("peer authentication key is invalid"))?;
    if key.proc_start != peer.proc_start
        || key.pid_domain != peer.pid_domain
        || key.peer_token.len() != 32
        || !key.peer_token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("peer authentication key is stale or invalid");
    }
    timeout(Duration::from_secs(5), async {
        let mut stream = UnixStream::connect(&peer.messaging_socket_path).await?;
        if peer_pid(&stream)? != peer.pid {
            bail!("peer socket endpoint does not match its registered process");
        }
        let auth = serde_json::to_vec(&json!({"type": "auth", "token": key.peer_token}))?;
        stream.write_all(&auth).await?;
        stream.write_all(b"\n").await?;
        stream.write_all(&line).await?;
        stream.write_all(b"\n").await?;
        stream.shutdown().await?;
        Ok(())
    })
    .await?
}

pub(crate) async fn read_frame(reader: &mut BufReader<UnixStream>) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    let length = timeout(
        Duration::from_secs(5),
        reader
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes),
    )
    .await??;
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_FRAME_BYTES || bytes.last() != Some(&b'\n') {
        bail!("peer frame exceeds size limit or has no newline");
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("invalid peer JSON frame"))
}
