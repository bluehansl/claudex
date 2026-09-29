//! Claude peer 수신을 기존 thread lifecycle과 자동 턴 시작 경계에 연결한다.

use crate::CodexThread;
use crate::ThreadManager;
use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::context::claude_peer_message::ClaudePeerMessage;
use anyhow::Result;
use codex_claude_peer::InboundPolicy;
use codex_claude_peer::Peer;
use codex_claude_peer::PeerOptions;
use codex_claude_peer::PermissionMode;
use codex_config::config_toml::ClaudePeerInbound;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadIdleCause;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadReadyInput;
use codex_extension_api::ThreadResumeInput;
use codex_extension_api::ThreadStopInput;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::turn_input::StartIfIdleSubmission;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnStartOptions;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Default)]
struct Owner {
    requested: Option<ThreadId>,
    current: Weak<Peer>,
}

static OWNER: OnceLock<Mutex<Owner>> = OnceLock::new();
static CANDIDATES: OnceLock<
    std::sync::Mutex<std::collections::HashMap<ThreadId, Weak<CodexThread>>>,
> = OnceLock::new();

pub(crate) struct PeerRuntime {
    pub(crate) peer: Arc<Peer>,
    dispatch: tokio::sync::Semaphore,
    admission: std::sync::Mutex<Option<(Option<PermissionMode>, bool)>>,
}

impl PeerRuntime {
    pub(crate) fn permits_mode(&self, receiver: PermissionMode) -> bool {
        self.admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|(sender, released)| self.peer.permits_mode(sender, released, receiver))
    }
}

fn is_recorded_message(
    item: &codex_protocol::models::ResponseItem,
    message: &codex_claude_peer::ReceivedMessage,
) -> bool {
    let expected = ClaudePeerMessage(message.clone()).render();
    matches!(item, codex_protocol::models::ResponseItem::Message { role, content, .. }
        if role == "assistant" && content.iter().any(|part| matches!(part,
            codex_protocol::models::ContentItem::InputText { text }
                | codex_protocol::models::ContentItem::OutputText { text } if text == &expected)))
}

pub(crate) async fn confirm_recorded(
    session: &crate::session::session::Session,
    items: &[codex_protocol::models::ResponseItem],
) -> Result<()> {
    let Some(runtime) = session.services.thread_extension_data.get::<PeerRuntime>() else {
        return Ok(());
    };
    if !items.iter().any(|item| matches!(item, codex_protocol::models::ResponseItem::Message { role, .. } if role == "assistant")) { return Ok(()); }
    for (seq, message) in runtime.peer.processing().await? {
        if items.iter().any(|item| is_recorded_message(item, &message)) {
            session.flush_rollout().await?;
            runtime.peer.delivered(seq, &message).await?;
        }
    }
    Ok(())
}

struct Contributor {
    manager: Weak<ThreadManager>,
}

/// 사용자 큐 뒤, 자동 goal 앞에 peer lifecycle을 등록한다.
pub fn install(registry: &mut ExtensionRegistryBuilder<Config>, manager: Weak<ThreadManager>) {
    registry.thread_lifecycle_contributor(Arc::new(Contributor { manager }));
}

pub(crate) fn permission_mode(config: &crate::ThreadConfigSnapshot) -> PermissionMode {
    if config.approval_policy == AskForApproval::Never
        && matches!(config.sandbox_policy(), SandboxPolicy::DangerFullAccess)
    {
        PermissionMode::Bypass
    } else {
        PermissionMode::Prompting
    }
}

async fn warning(thread: &CodexThread, message: String) {
    thread
        .session
        .send_event_raw(Event {
            id: "claude-peer".into(),
            msg: EventMsg::Warning(WarningEvent { message }),
        })
        .await;
}

/// TUI가 선택한 primary thread만 공개 peer로 전환한다.
#[expect(
    clippy::await_holding_invalid_type,
    reason = "primary switch must serialize registry replacement, history recovery and previous peer shutdown"
)]
pub async fn activate(thread_id: ThreadId) -> Result<()> {
    let mut owner = OWNER
        .get_or_init(|| Mutex::new(Owner::default()))
        .lock()
        .await;
    owner.requested = Some(thread_id);
    let thread = CANDIDATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&thread_id)
        .and_then(Weak::upgrade);
    let Some(thread) = thread else {
        return Ok(());
    };
    let config = thread.config().await;
    let Some(name) = &config.claude_peer_name else {
        return Ok(());
    };
    if let Some(runtime) = thread.thread_extension_data().get::<PeerRuntime>()
        && !runtime.peer.is_closed()
    {
        return Ok(());
    }
    let policy = match config.claude_peer_inbound {
        ClaudePeerInbound::Parity => InboundPolicy::Parity,
        ClaudePeerInbound::Accept => InboundPolicy::Accept,
        ClaudePeerInbound::Hold => InboundPolicy::Hold,
        ClaudePeerInbound::Refuse => InboundPolicy::Refuse,
    };
    let options = PeerOptions {
        claude_home: config.claude_config_dir.clone(),
        socket_directory: PathBuf::from("/tmp/cc-socks"),
        inbox_path: config
            .codex_home
            .join("claude-peer")
            .join(format!("{thread_id}.sqlite"))
            .to_path_buf(),
        session_id: thread_id.to_string(),
        name: name.clone(),
        cwd: config.cwd.to_string_lossy().into_owned(),
        mode: permission_mode(&thread.config_snapshot().await),
        policy,
    };
    let previous = owner.current.upgrade();
    let peer = match previous.as_deref().filter(|peer| !peer.is_closed()) {
        Some(previous) => Peer::replace(options, previous).await?,
        None => Peer::start(options).await?,
    };
    let identity = peer.identity().await;
    let recovery = async {
        let processing = peer.processing().await?;
        if !processing.is_empty() {
            let history = thread.load_history(false).await?;
            for (seq, message) in processing {
                if history.items.iter().any(|item| matches!(item,
                        codex_history::RolloutItem::ResponseItem(envelope) if is_recorded_message(&envelope.item, &message)))
                    { peer.delivered(seq, &message).await?; }
                    else { peer.release(seq).await?; }
            }
        }
        Ok::<(), anyhow::Error>(())
    }.await;
    if let Err(error) = recovery {
        peer.shutdown().await;
        if let Some(previous) = &previous {
            previous.restore_registration().await?;
        }
        return Err(error);
    }
    if let Some(previous) = previous {
        previous.shutdown().await;
    }
    owner.current = Arc::downgrade(&peer);
    drop(owner);
    thread.thread_extension_data().insert(PeerRuntime {
        peer: Arc::clone(&peer),
        dispatch: tokio::sync::Semaphore::new(1),
        admission: std::sync::Mutex::new(None),
    });
    warning(
        &thread,
        format!(
            "Claude cross-session peer: {} [{}]",
            identity.name,
            identity.reference()
        ),
    )
    .await;
    let weak_thread = Arc::downgrade(&thread);
    tokio::spawn(async move {
        while !peer.is_closed() {
            let Some(thread) = weak_thread.upgrade() else {
                break;
            };
            let closing = thread.session.is_shutting_down().await;
            if closing {
                break;
            }
            let busy = thread.session.active_turn.lock().await.is_some();
            let config = thread.config_snapshot().await;
            if let Err(error) = peer.set_status(busy, permission_mode(&config)).await {
                tracing::warn!("peer status update failed: {error}");
            }
            for notice in peer.take_notices().await {
                warning(&thread, notice).await;
            }
            if !busy && peer.pending().await.ok().flatten().is_some() {
                thread
                    .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
                    .await;
            }
            drop(thread);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        peer.shutdown().await;
    });

    Ok(())
}

impl Contributor {
    async fn ready(&self, level: &str) -> Result<()> {
        let Some(manager) = self.manager.upgrade() else {
            return Ok(());
        };
        let thread_id = ThreadId::from_string(level)?;
        let thread = manager.get_thread(thread_id).await?;
        let config = thread.config().await;
        if config.claude_peer_name.is_none()
            || !matches!(
                thread.session_source,
                SessionSource::Cli | SessionSource::Exec
            )
        {
            return Ok(());
        }
        anyhow::ensure!(
            !config.ephemeral,
            "Claude peer messaging requires a persistent session"
        );
        {
            let mut candidates = CANDIDATES
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            candidates.retain(|_, candidate| candidate.strong_count() != 0);
            candidates.insert(thread_id, Arc::downgrade(&thread));
        }
        let requested = OWNER
            .get_or_init(|| Mutex::new(Owner::default()))
            .lock()
            .await
            .requested;
        if requested == Some(thread_id) || matches!(thread.session_source, SessionSource::Exec) {
            activate(thread_id).await?;
        }
        Ok(())
    }

    async fn dispatch(&self, input: ThreadIdleInput<'_>) -> Result<()> {
        if input.cause != ThreadIdleCause::Completed {
            return Ok(());
        }
        let Some(runtime) = input.thread_store.get::<PeerRuntime>() else {
            return Ok(());
        };
        let Ok(_dispatch) = runtime.dispatch.try_acquire() else {
            return Ok(());
        };
        if runtime.peer.is_closed() {
            return Ok(());
        }
        let Some(manager) = self.manager.upgrade() else {
            return Ok(());
        };
        let thread = manager
            .get_thread(ThreadId::from_string(input.thread_store.level_id())?)
            .await?;
        if thread.session.is_interrupted() || thread.session.is_shutting_down().await {
            return Ok(());
        }
        let Some((seq, message)) = runtime.peer.pending().await? else {
            return Ok(());
        };
        if runtime.peer.is_closed()
            || thread.session.is_interrupted()
            || thread.session.is_shutting_down().await
        {
            return Ok(());
        }
        if !runtime
            .peer
            .revalidate(
                seq,
                &message,
                permission_mode(&thread.config_snapshot().await),
            )
            .await?
        {
            return Ok(());
        }
        runtime.peer.claim(seq, &message).await?;
        *runtime
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((message.mode, message.approval_released));
        let request = TurnInputRequest::new(TurnInput::ResponseItem(ContextualUserFragment::into(
            ClaudePeerMessage(message.clone()),
        )))
        .on_start(TurnStartOptions {
            turn_trigger: Some("claude_peer".into()),
            ..Default::default()
        });
        match thread.start_turn_if_idle(request).await {
            Ok(StartIfIdleSubmission::Started { .. }) => {}
            Ok(StartIfIdleSubmission::NotSubmitted { .. }) => runtime.peer.release(seq).await?,
            Err(error) => {
                runtime.peer.release(seq).await?;
                return Err(error.into());
            }
        }
        Ok(())
    }
}

impl ThreadLifecycleContributor<Config> for Contributor {
    fn on_thread_ready<'a>(
        &'a self,
        input: ThreadReadyInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Err(error) = self.ready(input.thread_store.level_id()).await {
                tracing::warn!("Claude peer registration failed: {error}");
                if let Some(manager) = self.manager.upgrade()
                    && let Ok(id) = ThreadId::from_string(input.thread_store.level_id())
                    && let Ok(thread) = manager.get_thread(id).await
                {
                    warning(&thread, format!("Claude peer registration failed: {error}")).await;
                }
            }
        })
    }

    fn on_thread_resume<'a>(&'a self, input: ThreadResumeInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Err(error) = self.ready(input.thread_store.level_id()).await {
                tracing::warn!("peer resume failed: {error}");
            }
        })
    }

    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Err(error) = self.dispatch(input).await {
                tracing::warn!("peer dispatch failed: {error}");
            }
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(runtime) = input.thread_store.get::<PeerRuntime>() {
                runtime.peer.shutdown().await;
            }
        })
    }
}
