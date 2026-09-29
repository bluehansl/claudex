use crate::protocol::InboundPolicy;
use crate::protocol::PermissionMode;
use crate::protocol::ReceivedMessage;
use crate::protocol::control_frame;
use crate::protocol::user_frame;
use crate::receive::Guard;
use crate::registry;
use crate::registry::PeerIdentity;
use crate::registry::Registration;
use crate::store::Inbox;
use crate::transport;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct PeerOptions {
    pub claude_home: PathBuf,
    pub socket_directory: PathBuf,
    pub inbox_path: PathBuf,
    pub session_id: String,
    pub name: String,
    pub cwd: String,
    pub mode: PermissionMode,
    pub policy: InboundPolicy,
}

pub(crate) struct Subscription {
    pub peer: PeerIdentity,
    pub message_id: String,
    pub created_at: i64,
}

pub struct Peer {
    pub(crate) root: PathBuf,
    pub(crate) identity: RwLock<PeerIdentity>,
    registration: Mutex<Option<Registration>>,
    pub(crate) token: String,
    pub(crate) mode: RwLock<PermissionMode>,
    pub(crate) policy: InboundPolicy,
    pub(crate) inbox: Inbox,
    pub(crate) guard: Mutex<Guard>,
    pub(crate) inbound_order: tokio::sync::Semaphore,
    pub(crate) subscriptions: Mutex<Vec<Subscription>>,
    pub(crate) receipts: Mutex<BTreeMap<String, Value>>,
    pub(crate) notices: Mutex<VecDeque<String>>,
    hops: Mutex<Vec<String>>,
    pub(crate) own_hop: String,
    pub(crate) cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
    closing: Mutex<()>,
}

impl Peer {
    pub fn is_closed(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn stop_accepting(&self) {
        self.cancel.cancel();
    }

    pub fn permits_mode(
        &self,
        sender: Option<PermissionMode>,
        released: bool,
        receiver: PermissionMode,
    ) -> bool {
        !self.is_closed()
            && (released || crate::protocol::admission(self.policy, receiver, sender) == "pending")
    }

    pub async fn restore_registration(&self) -> Result<()> {
        let identity = self.identity().await;
        if let Some(registration) = self.registration.lock().await.as_ref() {
            registration.update(&identity)?;
        }
        Ok(())
    }

    pub async fn start(options: PeerOptions) -> Result<Arc<Self>> {
        Self::start_inner(options, None).await
    }

    pub async fn replace(options: PeerOptions, previous: &Peer) -> Result<Arc<Self>> {
        Self::start_inner(options, Some(&previous.identity().await)).await
    }

    async fn start_inner(
        options: PeerOptions,
        previous: Option<&PeerIdentity>,
    ) -> Result<Arc<Self>> {
        if options.name.trim().is_empty()
            || options.name.len() > 64
            || options
                .name
                .chars()
                .any(|c| c.is_control() || "<>\"&".contains(c))
        {
            bail!("peer name must be 1..64 bytes without control or markup characters");
        }
        registry::private_directory(&options.socket_directory)?;
        let pid = std::process::id();
        let path = options.socket_directory.join(format!(
            "{pid}-{}.sock",
            &Uuid::new_v4().simple().to_string()[..8]
        ));
        let listener = UnixListener::bind(&path)?;
        let socket = registry::SocketGuard {
            path: path.clone(),
            inode: fs::symlink_metadata(&path)?.ino(),
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let now = chrono::Utc::now().timestamp_millis();
        let identity = PeerIdentity {
            pid,
            session_id: options.session_id,
            cwd: options.cwd,
            started_at: now,
            proc_start: registry::process_start(pid).await?,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            peer_protocol: 1,
            peer_features: vec!["notify_idle".to_owned()],
            kind: "interactive".into(),
            entrypoint: "cli".into(),
            pid_domain: registry::pid_domain().into(),
            messaging_socket_path: path,
            name: options.name,
            name_source: "user".into(),
            name_since: now,
            status: "busy".into(),
            updated_at: now,
            status_updated_at: now,
        };
        let inbox = Inbox::open(&options.inbox_path).await?;
        let root = options.claude_home.join("sessions");
        let registration = Registration::publish(root.clone(), identity.clone(), socket, previous)?;
        let peer = Arc::new(Self {
            root,
            identity: RwLock::new(identity),
            token: registration.token.clone(),
            registration: Mutex::new(Some(registration)),
            mode: RwLock::new(options.mode),
            policy: options.policy,
            inbox,
            guard: Mutex::new(Guard::default()),
            inbound_order: tokio::sync::Semaphore::new(1),
            subscriptions: Mutex::new(Vec::new()),
            receipts: Mutex::new(BTreeMap::new()),
            notices: Mutex::new(VecDeque::new()),
            hops: Mutex::new(Vec::new()),
            own_hop: Uuid::new_v4().simple().to_string()[..24].into(),
            cancel: CancellationToken::new(),
            task: Mutex::new(None),
            closing: Mutex::new(()),
        });
        let weak = Arc::downgrade(&peer);
        let cancel = peer.cancel.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break; };
                        let Some(peer) = weak.upgrade() else { break; };
                        if connections.len() < 32 {
                            connections.spawn(async move {
                                // 잘못된 입력은 원문이나 토큰을 로그로 남기지 않고 연결만 닫는다.
                                let _ = peer.receive_connection(stream).await;
                            });
                        }
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        *peer.task.lock().await = Some(task);
        Ok(peer)
    }

    pub async fn identity(&self) -> PeerIdentity {
        self.identity.read().await.clone()
    }

    pub async fn list_sessions(&self) -> Result<Vec<PeerIdentity>> {
        registry::list(&self.root).await
    }

    pub async fn send_message(
        &self,
        target: &str,
        message: &str,
        notify_when_idle: bool,
    ) -> Result<Value> {
        let recipient = registry::resolve(&self.root, target).await?;
        let identity = self.identity().await;
        if recipient.pid == identity.pid {
            bail!("cannot send peer message to this process");
        }
        let id = Uuid::new_v4().to_string();
        let mut hops = self.hops.lock().await.clone();
        hops.push(self.own_hop.clone());
        if hops.len() > 28 {
            bail!("peer relay chain limit reached");
        }
        let mode = *self.mode.read().await;
        let frame = user_frame(
            &id,
            &identity.address(),
            &identity.name,
            &identity.session_id,
            mode,
            message,
            &hops,
        )?;
        {
            let mut receipts = self.receipts.lock().await;
            if receipts.len() >= 200
                && let Some(key) = receipts.keys().next().cloned()
            {
                receipts.remove(&key);
            }
            receipts.insert(id.clone(), json!({"target": recipient.address(), "pid": recipient.pid, "start": recipient.proc_start, "status": "sending"}));
        }
        transport::send(&self.root, &recipient, &frame).await?;
        if let Some(receipt) = self.receipts.lock().await.get_mut(&id)
            && receipt["status"] == "sending"
        {
            receipt["status"] = json!("written");
        }
        let mut result = json!({"message_id": id, "target": recipient.address(), "status": "written", "meaning": "Socket write completed; model delivery and reply are asynchronous."});
        if notify_when_idle {
            if recipient
                .peer_features
                .iter()
                .any(|feature| feature == "notify_idle")
            {
                let frame = control_frame(
                    json!({"action": "notify_when_idle", "from": identity.address(), "from_mode": mode.as_str()}),
                );
                let subscription_id = frame["msg_id"].as_str().unwrap_or_default().to_owned();
                self.receipts.lock().await.insert(subscription_id.clone(), json!({"target": recipient.address(), "pid": recipient.pid, "start": recipient.proc_start, "status": "awaiting_idle"}));
                transport::send(&self.root, &recipient, &frame).await?;
                result["idle_subscription_id"] = json!(subscription_id);
            } else {
                result["idle_subscription"] = json!("unsupported");
            }
        }
        Ok(result)
    }

    pub async fn pending(&self) -> Result<Option<(i64, ReceivedMessage)>> {
        self.inbox.pending().await
    }

    pub async fn claim(&self, seq: i64, message: &ReceivedMessage) -> Result<()> {
        self.inbox.claim(seq, message).await
    }
    pub async fn release(&self, seq: i64) -> Result<()> {
        self.inbox.release(seq).await
    }
    pub async fn processing(&self) -> Result<Vec<(i64, ReceivedMessage)>> {
        self.inbox.processing().await
    }

    pub async fn revalidate(
        &self,
        seq: i64,
        message: &ReceivedMessage,
        mode: PermissionMode,
    ) -> Result<bool> {
        *self.mode.write().await = mode;
        if message.approval_released
            || crate::protocol::admission(self.policy, mode, message.mode) == "pending"
        {
            return Ok(true);
        }
        self.inbox.hold(seq).await?;
        self.receipt(message, "held", None).await;
        self.notice(format!(
            "Peer message {} was held after a permission-mode change.",
            message.id
        ))
        .await;
        Ok(false)
    }

    pub async fn delivered(&self, seq: i64, message: &ReceivedMessage) -> Result<()> {
        self.inbox.delivered(seq).await?;
        *self.hops.lock().await = message.hop_chain.clone();
        if message.approval_released {
            self.receipt(message, "delivered", None).await;
        }
        Ok(())
    }

    pub async fn held_messages(&self) -> Result<Vec<(i64, ReceivedMessage)>> {
        self.inbox.held().await
    }

    pub async fn decide(&self, seq: i64, approve: bool) -> Result<()> {
        self.inbox.decide(seq, approve).await?;
        Ok(())
    }

    pub async fn delivery_status(&self, id: &str) -> Option<Value> {
        self.receipts.lock().await.get(id).cloned()
    }

    pub async fn take_notices(&self) -> Vec<String> {
        self.notices.lock().await.drain(..).collect()
    }

    pub(crate) async fn notice(&self, text: String) {
        let mut notices = self.notices.lock().await;
        if notices.len() >= 100 {
            notices.pop_front();
        }
        notices.push_back(text);
    }

    pub async fn set_status(&self, busy: bool, mode: PermissionMode) -> Result<()> {
        *self.mode.write().await = mode;
        for (seq, message) in self.inbox.denied().await? {
            self.receipt(&message, "denied", None).await;
            self.inbox.receipt_sent(seq).await?;
        }
        let status = if busy { "busy" } else { "idle" };
        let changed_identity = {
            let mut identity = self.identity.write().await;
            if identity.status != status {
                identity.status = status.into();
                identity.updated_at = chrono::Utc::now().timestamp_millis();
                identity.status_updated_at = identity.updated_at;
                Some(identity.clone())
            } else {
                None
            }
        };
        if let Some(identity) = changed_identity
            && let Some(registration) = self.registration.lock().await.as_ref()
        {
            registration.update(&identity)?;
        }
        if !busy
            && self.inbox.pending().await?.is_none()
            && self.inbox.held().await?.is_empty()
            && self.inbox.processing().await?.is_empty()
        {
            self.notify_subscribers("idle").await;
            self.hops.lock().await.clear();
        }
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "bounded shutdown must serialize cleanup with replacement and concurrent shutdown callers"
    )]
    pub async fn shutdown(&self) {
        let _closing = self.closing.lock().await;
        self.cancel.cancel();
        let task = { self.task.lock().await.take() };
        if let Some(task) = task {
            let _ = task.await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            self.notify_subscribers("exited").await;
            if let Ok(messages) = self.inbox.held().await {
                for (seq, message) in messages {
                    let _ = self.inbox.decide(seq, false).await;
                    self.receipt(&message, "expired", None).await;
                    let _ = self.inbox.receipt_sent(seq).await;
                }
            }
        })
        .await;
        self.registration.lock().await.take();
    }

    async fn notify_subscribers(&self, state: &str) {
        let subscribers = std::mem::take(&mut *self.subscriptions.lock().await);
        let identity = self.identity().await;
        let mode = *self.mode.read().await;
        for subscriber in subscribers {
            if chrono::Utc::now().timestamp_millis() - subscriber.created_at > 43_200_000 {
                continue;
            }
            let frame = control_frame(
                json!({"action": "peer_idle_notice", "orig_msg_id": subscriber.message_id, "state": state, "finished_at": chrono::Utc::now().to_rfc3339(), "from": identity.address(), "from_mode": mode.as_str()}),
            );
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                transport::send(&self.root, &subscriber.peer, &frame),
            )
            .await;
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
