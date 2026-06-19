use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::Path;

const MESSAGE_TYPE: &str = "message";
const SHUTDOWN_REQUEST_TYPE: &str = "shutdown_request";
const SHUTDOWN_APPROVED_TYPE: &str = "shutdown_approved";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ClaudeTeamEnvelope {
    pub(crate) from: String,
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    pub(crate) timestamp: String,
    #[serde(rename = "type")]
    pub(crate) message_type: String,
    #[serde(default)]
    pub(crate) read: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub(crate) enum ClaudeTeamControlMessage {
    #[serde(rename = "shutdown_request")]
    ShutdownRequest {
        #[serde(rename = "requestId")]
        request_id: String,
        from: String,
        reason: String,
        timestamp: String,
    },
    #[serde(rename = "shutdown_approved")]
    ShutdownApproved {
        #[serde(rename = "requestId")]
        request_id: String,
        from: String,
        timestamp: String,
        #[serde(rename = "paneId")]
        pane_id: String,
        #[serde(rename = "backendType")]
        backend_type: String,
    },
}

impl ClaudeTeamEnvelope {
    pub(crate) fn plain_message(from: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            from: from.into(),
            summary: Some(summarize_message(&text)),
            text,
            timestamp: timestamp_now(),
            message_type: MESSAGE_TYPE.to_string(),
            read: false,
        }
    }

    pub(crate) fn shutdown_approved(
        from: impl Into<String>,
        request_id: impl Into<String>,
        pane_id: impl Into<String>,
    ) -> Result<Self> {
        let from = from.into();
        let timestamp = timestamp_now();
        let control = ClaudeTeamControlMessage::ShutdownApproved {
            request_id: request_id.into(),
            from: from.clone(),
            timestamp: timestamp.clone(),
            pane_id: pane_id.into(),
            backend_type: "tmux".to_string(),
        };
        Ok(Self {
            from,
            text: serde_json::to_string(&control)?,
            summary: None,
            timestamp,
            message_type: MESSAGE_TYPE.to_string(),
            read: false,
        })
    }

    pub(crate) fn as_control_message(&self) -> Option<ClaudeTeamControlMessage> {
        serde_json::from_str(&self.text).ok()
    }

    pub(crate) fn to_teammate_response_item(&self) -> ResponseItem {
        ResponseItem::from(ResponseInputItem::Message {
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: self.to_teammate_message_text(),
            }],
            phase: None,
        })
    }

    pub(crate) fn to_teammate_message_text(&self) -> String {
        let summary = self
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
            .unwrap_or_default();
        format!(
            "<teammate-message teammate_id=\"{}\" summary=\"{}\">\n{}\n</teammate-message>",
            escape_xml_attr(&self.from),
            escape_xml_attr(summary),
            self.text
        )
    }

    pub(crate) fn is_plain_message(&self) -> bool {
        self.message_type == MESSAGE_TYPE && self.as_control_message().is_none()
    }

    pub(crate) fn is_shutdown_request(&self) -> bool {
        matches!(
            self.as_control_message(),
            Some(ClaudeTeamControlMessage::ShutdownRequest { .. })
        )
    }
}

pub(crate) fn read_inbox(path: &Path) -> Result<Vec<ClaudeTeamEnvelope>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

pub(crate) fn write_inbox_atomic(path: &Path, envelopes: &[ClaudeTeamEnvelope]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("inbox path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create inbox dir {}", parent.display()))?;

    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temp file in {}", parent.display()))?;
    serde_json::to_writer_pretty(&mut temp, envelopes)
        .with_context(|| format!("failed to serialize inbox {}", path.display()))?;
    temp.write_all(b"\n")
        .with_context(|| format!("failed to write newline for {}", path.display()))?;
    temp.as_file_mut()
        .sync_all()
        .with_context(|| format!("failed to sync temp inbox {}", path.display()))?;
    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to replace inbox {}", path.display()))?;
    Ok(())
}

pub(crate) fn append_inbox(
    path: &Path,
    envelope: ClaudeTeamEnvelope,
) -> Result<Vec<ClaudeTeamEnvelope>> {
    let mut envelopes = read_inbox(path)?;
    envelopes.push(envelope);
    write_inbox_atomic(path, &envelopes)?;
    Ok(envelopes)
}

fn timestamp_now() -> String {
    timestamp_to_string(Utc::now())
}

fn timestamp_to_string(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn summarize_message(text: &str) -> String {
    let mut words = text.split_whitespace().take(10).collect::<Vec<_>>();
    if words.is_empty() {
        return String::new();
    }
    let mut summary = words.join(" ");
    const MAX_SUMMARY_CHARS: usize = 80;
    if summary.chars().count() > MAX_SUMMARY_CHARS {
        words.clear();
        let mut truncated = String::new();
        for ch in summary.chars().take(MAX_SUMMARY_CHARS - 1) {
            truncated.push(ch);
        }
        truncated.push_str("...");
        summary = truncated;
    }
    summary
}

fn escape_xml_attr(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '"' => escaped.push_str("&quot;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn plain_message_envelope_matches_claude_shape() {
        let envelope = ClaudeTeamEnvelope::plain_message("claudex", "hello from claudex");
        let value = serde_json::to_value(&envelope).expect("serialize envelope");

        assert_eq!(value["from"], "claudex");
        assert_eq!(value["text"], "hello from claudex");
        assert_eq!(value["summary"], "hello from claudex");
        assert_eq!(value["type"], "message");
        assert_eq!(value["read"], false);
        assert!(value.get("id").is_none());
        assert!(value.get("color").is_none());
    }

    #[test]
    fn teammate_wrapper_uses_user_visible_claude_format() {
        let envelope = ClaudeTeamEnvelope {
            from: "lead\"<&>".to_string(),
            text: "keep\nbody".to_string(),
            summary: Some("summary\"<&>".to_string()),
            timestamp: "2026-06-18T00:00:00.000Z".to_string(),
            message_type: MESSAGE_TYPE.to_string(),
            read: false,
        };

        assert_eq!(
            envelope.to_teammate_message_text(),
            "<teammate-message teammate_id=\"lead&quot;&lt;&amp;&gt;\" summary=\"summary&quot;&lt;&amp;&gt;\">\nkeep\nbody\n</teammate-message>"
        );
    }

    #[test]
    fn shutdown_request_nested_json_is_detected() {
        let envelope = ClaudeTeamEnvelope {
            from: "lead".to_string(),
            text: json!({
                "type": "shutdown_request",
                "requestId": "shutdown-1@claudex",
                "from": "lead",
                "reason": "done",
                "timestamp": "2026-06-18T00:00:00.000Z"
            })
            .to_string(),
            summary: None,
            timestamp: "2026-06-18T00:00:00.000Z".to_string(),
            message_type: MESSAGE_TYPE.to_string(),
            read: false,
        };

        assert!(envelope.is_shutdown_request());
        assert!(!envelope.is_plain_message());
    }

    #[test]
    fn shutdown_approved_uses_captured_on_disk_type() {
        let envelope = ClaudeTeamEnvelope::shutdown_approved(
            "claudex",
            "shutdown-1@claudex",
            "claudex:session",
        )
        .expect("build shutdown approval");

        assert_eq!(envelope.message_type, MESSAGE_TYPE);
        let control = envelope
            .as_control_message()
            .expect("shutdown approval should parse");
        assert_eq!(
            control,
            ClaudeTeamControlMessage::ShutdownApproved {
                request_id: "shutdown-1@claudex".to_string(),
                from: "claudex".to_string(),
                timestamp: envelope.timestamp,
                pane_id: "claudex:session".to_string(),
                backend_type: "tmux".to_string(),
            }
        );
    }

    #[test]
    fn inbox_read_append_and_replace_round_trips() {
        let temp = tempdir().expect("tempdir");
        let inbox = temp.path().join("teams/session-test/inboxes/worker.json");

        assert_eq!(read_inbox(&inbox).expect("missing inbox"), Vec::new());

        let first = ClaudeTeamEnvelope::plain_message("lead", "one");
        let second = ClaudeTeamEnvelope::plain_message("lead", "two");
        append_inbox(&inbox, first.clone()).expect("append first");
        append_inbox(&inbox, second.clone()).expect("append second");

        assert_eq!(read_inbox(&inbox).expect("read inbox"), vec![first, second]);

        write_inbox_atomic(&inbox, &[]).expect("clear inbox");
        assert_eq!(read_inbox(&inbox).expect("read cleared inbox"), Vec::new());
    }
}
