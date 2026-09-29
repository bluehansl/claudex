use crate::protocol::ReceivedMessage;
use crate::protocol::UserFrame;
use crate::protocol::admission;
use crate::protocol::control_frame;
use crate::protocol::parse_envelope;
use crate::registry;
use crate::registry::PeerIdentity;
use crate::service::Peer;
use crate::service::Subscription;
use crate::transport;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use subtle::ConstantTimeEq;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use uuid::Uuid;

struct SenderGuard {
    tokens: f64,
    last_seen: Instant,
}

#[derive(Default)]
pub(crate) struct Guard {
    senders: HashMap<String, SenderGuard>,
}

impl Guard {
    fn admit(&mut self, sender: &str) -> Option<&'static str> {
        let now = Instant::now();
        if !self.senders.contains_key(sender) && self.senders.len() >= 128 {
            let oldest = self
                .senders
                .iter()
                .min_by_key(|(_, value)| value.last_seen)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                self.senders.remove(&key);
            }
        }
        let guard = self
            .senders
            .entry(sender.to_owned())
            .or_insert_with(|| SenderGuard {
                tokens: 30.0,
                last_seen: now,
            });
        guard.tokens =
            (guard.tokens + now.duration_since(guard.last_seen).as_secs_f64() * 0.5).min(30.0);
        guard.last_seen = now;
        if guard.tokens < 1.0 {
            return Some("rate-limited");
        }
        guard.tokens -= 1.0;
        None
    }
}

impl Peer {
    pub(crate) async fn receive_connection(self: &Arc<Self>, stream: UnixStream) -> Result<()> {
        let pid = transport::peer_pid(&stream)?;
        let mut reader = BufReader::new(stream);
        let Some(auth) = transport::read_frame(&mut reader).await? else {
            return Ok(());
        };
        let token = auth
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if auth.get("type").and_then(Value::as_str) != Some("auth")
            || token.len() != 32
            || !bool::from(self.token.as_bytes().ct_eq(token.as_bytes()))
        {
            bail!("peer authentication failed");
        }
        let bytes =
            registry::read_owned_file(&self.root.join(format!("{pid}.json")), 16_384, false)?;
        let sender: PeerIdentity = serde_json::from_slice(&bytes)?;
        if sender.pid != pid || !sender.is_live().await {
            bail!("sender registry is stale");
        }
        for _ in 0..16 {
            let Some(frame) = transport::read_frame(&mut reader).await? else {
                break;
            };
            if self.cancel.is_cancelled() {
                break;
            }
            if let Some(from) = frame.get("from").and_then(Value::as_str)
                && from != sender.address()
            {
                bail!("sender address does not match authenticated peer");
            }
            if let Some(session) = frame.get("session_id").and_then(Value::as_str)
                && session != self.identity.read().await.session_id
            {
                bail!("peer message targets another session");
            }
            match frame.get("type").and_then(Value::as_str) {
                Some("user") => self.receive_user(frame, &sender).await?,
                Some("control") => self.receive_control(frame, &sender).await?,
                _ => bail!("unsupported peer frame"),
            }
        }
        Ok(())
    }

    async fn receive_user(&self, frame: Value, sender: &PeerIdentity) -> Result<()> {
        let frame: UserFrame = serde_json::from_value(frame)?;
        if frame.msg_v.is_some_and(|version| version != 1) {
            bail!("unsupported peer version");
        }
        let id = frame
            .msg_id
            .or(frame.uuid)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        Uuid::parse_str(&id)?;
        let envelope = parse_envelope(&frame.message.content)?;
        if envelope.body.trim().is_empty() {
            bail!("peer message body is empty");
        }
        let message = ReceivedMessage {
            id,
            sender: sender.address(),
            sender_session: sender.session_id.clone(),
            sender_name: sender.name.clone(),
            sender_pid: sender.pid,
            sender_start: sender.proc_start.clone(),
            body: envelope.body,
            mode: envelope.mode,
            hop_chain: envelope.hop_chain,
            approval_released: false,
        };
        if frame.file_attachments.is_some() {
            self.receipt(&message, "refused", None).await;
            return Ok(());
        }
        let state = admission(self.policy, *self.mode.read().await, message.mode);
        if state == "refused" {
            self.receipt(&message, "refused", None).await;
            return Ok(());
        }
        let reason = if message
            .hop_chain
            .iter()
            .filter(|hop| *hop == &self.own_hop)
            .count()
            >= 10
        {
            Some("hop-loop")
        } else if message.hop_chain.len() > 28 {
            Some("hop-runaway")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.receipt(&message, "dropped", Some(reason)).await;
            return Ok(());
        }
        // 인증을 마친 연결의 수신 순서대로 admission과 영속화를 직렬화한다.
        let result = {
            let _order = self.inbound_order.acquire().await?;
            let key = format!("{}:{}", sender.pid, sender.proc_start);
            let reason = { self.guard.lock().await.admit(&key) };
            if let Some(reason) = reason {
                reason
            } else {
                self.inbox.receive(&message, state).await?
            }
        };
        match result {
            "accepted" if state == "held" => {
                self.receipt(&message, "held", None).await;
                self.notice(format!(
                    "Peer message from {} is held for approval ({}).",
                    message.sender_name, message.id
                ))
                .await;
            }
            "accepted" => {}
            reason => self.receipt(&message, "dropped", Some(reason)).await,
        }
        Ok(())
    }

    async fn receive_control(&self, frame: Value, sender: &PeerIdentity) -> Result<()> {
        match frame.get("action").and_then(Value::as_str) {
            Some("notify_when_idle") => {
                let id = frame
                    .get("msg_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing subscription id"))?;
                Uuid::parse_str(id)?;
                let mode = frame
                    .get("from_mode")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?;
                let allowed = admission(self.policy, *self.mode.read().await, mode) == "pending";
                let unavailable = {
                    let mut subscriptions = self.subscriptions.lock().await;
                    subscriptions.retain(|entry| {
                        chrono::Utc::now().timestamp_millis() - entry.created_at < 43_200_000
                    });
                    if subscriptions
                        .iter()
                        .any(|entry| entry.message_id == id && entry.peer.pid == sender.pid)
                    {
                        return Ok(());
                    }
                    if !allowed
                        || subscriptions.len() >= 32
                        || subscriptions
                            .iter()
                            .filter(|entry| entry.peer.pid == sender.pid)
                            .count()
                            >= 4
                    {
                        true
                    } else {
                        subscriptions.push(Subscription {
                            peer: sender.clone(),
                            message_id: id.into(),
                            created_at: chrono::Utc::now().timestamp_millis(),
                        });
                        false
                    }
                };
                if unavailable {
                    let notice = control_frame(
                        json!({"action": "peer_idle_notice", "orig_msg_id": id, "state": "unavailable", "from": self.identity().await.address(), "from_mode": self.mode.read().await.as_str()}),
                    );
                    let _ = transport::send(&self.root, sender, &notice).await;
                }
            }
            Some("peer_message_status" | "peer_idle_notice") => {
                let Some(id) = frame.get("orig_msg_id").and_then(Value::as_str) else {
                    return Ok(());
                };
                let status = {
                    let mut receipts = self.receipts.lock().await;
                    let Some(receipt) = receipts.get_mut(id) else {
                        return Ok(());
                    };
                    if receipt["pid"] != sender.pid || receipt["start"] != sender.proc_start {
                        return Ok(());
                    }
                    let status = frame
                        .get("status")
                        .or_else(|| frame.get("state"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !matches!(
                        status,
                        "held"
                            | "denied"
                            | "expired"
                            | "delivered"
                            | "refused"
                            | "dropped"
                            | "idle"
                            | "exited"
                            | "unavailable"
                    ) {
                        return Ok(());
                    }
                    if frame["action"] == "peer_idle_notice" && receipt["status"] != "awaiting_idle"
                    {
                        return Ok(());
                    }
                    if status == "held"
                        && receipt["status"] != "written"
                        && receipt["status"] != "sending"
                    {
                        return Ok(());
                    }
                    receipt["status"] = json!(status);
                    status.to_owned()
                };
                self.notice(format!("Peer {}: {status} ({id}).", sender.name))
                    .await;
            }
            // rename/shutdown/artifact 제어는 이 어댑터의 권한에 포함하지 않는다.
            _ => {}
        }
        Ok(())
    }

    pub(crate) async fn receipt(
        &self,
        message: &ReceivedMessage,
        status: &str,
        reason: Option<&str>,
    ) {
        let Ok(recipient) = registry::resolve(&self.root, &message.sender).await else {
            return;
        };
        if recipient.pid != message.sender_pid || recipient.proc_start != message.sender_start {
            return;
        }
        let mut fields = json!({"action": "peer_message_status", "status": status, "orig_msg_id": message.id, "from": self.identity().await.address()});
        if let Some(reason) = reason {
            fields["drop_reason"] = json!(reason);
        }
        let _ = transport::send(&self.root, &recipient, &control_frame(fields)).await;
    }
}
