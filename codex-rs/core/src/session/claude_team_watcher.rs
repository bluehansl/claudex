use super::session::Session;
use crate::claude_team_protocol::ClaudeTeamBinding;
use crate::claude_team_protocol::ClaudeTeamControlMessage;
use crate::claude_team_protocol::ClaudeTeamEnvelope;
use crate::claude_team_protocol::append_inbox;
use crate::claude_team_protocol::read_inbox;
use crate::claude_team_protocol::write_inbox_atomic;
use anyhow::Context;
use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::debug;
use tracing::warn;

const POLL_INTERVAL: Duration = Duration::from_millis(750);

pub(super) fn spawn_claude_team_inbox_watcher(session: &Arc<Session>, binding: ClaudeTeamBinding) {
    let session = Arc::downgrade(session);
    tokio::spawn(async move {
        let mut last_fingerprint = None;
        loop {
            let Some(session) = session.upgrade() else {
                break;
            };
            let inbox_path = binding.self_inbox_path();
            match inbox_fingerprint(inbox_path.as_path()) {
                Ok(fingerprint) if fingerprint != last_fingerprint => {
                    if let Err(err) = process_claude_team_inbox_once(&session, &binding).await {
                        warn!("failed to process Claude team inbox: {err:#}");
                    }
                    last_fingerprint =
                        inbox_fingerprint(inbox_path.as_path()).unwrap_or(fingerprint);
                }
                Ok(_) => {}
                Err(err) => {
                    debug!("failed to fingerprint Claude team inbox: {err:#}");
                }
            }
            time::sleep(POLL_INTERVAL).await;
        }
    });
}

pub(super) async fn process_claude_team_inbox_once(
    session: &Arc<Session>,
    binding: &ClaudeTeamBinding,
) -> Result<()> {
    let inbox_path = binding.self_inbox_path();
    let envelopes = read_inbox(&inbox_path)?;
    if envelopes.is_empty() {
        return Ok(());
    }

    let mut remaining = Vec::new();
    for envelope in envelopes {
        if envelope.is_plain_message() {
            session
                .input_queue
                .enqueue_mailbox_response_item(
                    envelope.to_teammate_response_item(),
                    /*trigger_turn*/ true,
                )
                .await;
            continue;
        }

        if handle_shutdown_request(&envelope, binding)? {
            continue;
        }

        remaining.push(envelope);
    }

    write_inbox_atomic(&inbox_path, &remaining)?;
    if session.input_queue.has_trigger_turn_mailbox_items().await {
        session.maybe_start_turn_for_pending_work().await;
    }
    Ok(())
}

fn handle_shutdown_request(
    envelope: &ClaudeTeamEnvelope,
    binding: &ClaudeTeamBinding,
) -> Result<bool> {
    let Some(ClaudeTeamControlMessage::ShutdownRequest {
        request_id, from, ..
    }) = envelope.as_control_message()
    else {
        return Ok(false);
    };

    let approval =
        ClaudeTeamEnvelope::shutdown_approved(binding.agent_id(), request_id, binding.pane_id())?;
    append_inbox(&binding.inbox_path_for_agent(&from), approval)?;
    Ok(true)
}

fn inbox_fingerprint(path: &Path) -> Result<Option<u64>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(Some(hasher.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[test]
    fn missing_inbox_has_no_fingerprint() {
        let temp = tempdir().expect("tempdir");

        assert_eq!(
            inbox_fingerprint(&temp.path().join("missing.json")).expect("fingerprint"),
            None
        );
    }

    #[test]
    fn inbox_fingerprint_changes_with_content() {
        let temp = tempdir().expect("tempdir");
        let inbox = temp.path().join("inboxes/worker.json");
        write_inbox_atomic(&inbox, &[ClaudeTeamEnvelope::plain_message("lead", "one")])
            .expect("write first");
        let first = inbox_fingerprint(&inbox).expect("first fingerprint");

        write_inbox_atomic(&inbox, &[ClaudeTeamEnvelope::plain_message("lead", "two")])
            .expect("write second");

        assert_ne!(
            first,
            inbox_fingerprint(&inbox).expect("second fingerprint")
        );
    }
}
