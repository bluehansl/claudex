use anyhow::Result;
use anyhow::bail;
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

pub const MAX_FRAME_BYTES: usize = 65_536;
pub const MAX_MESSAGE_BYTES: usize = 8_192;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundPolicy {
    #[default]
    Parity,
    Accept,
    Hold,
    Refuse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Bypass,
    Prompting,
}

impl PermissionMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Bypass => "bypass",
            Self::Prompting => "prompting",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedMessage {
    pub id: String,
    pub sender: String,
    pub sender_session: String,
    pub sender_name: String,
    pub sender_pid: u32,
    pub sender_start: String,
    pub body: String,
    pub mode: Option<PermissionMode>,
    pub hop_chain: Vec<String>,
    #[serde(default)]
    pub approval_released: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct UserFrame {
    #[serde(rename = "msgV")]
    pub msg_v: Option<u32>,
    pub msg_id: Option<String>,
    pub uuid: Option<String>,
    pub message: MessageBody,
    pub file_attachments: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct MessageBody {
    pub content: String,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Envelope {
    pub body: String,
    pub mode: Option<PermissionMode>,
    pub hop_chain: Vec<String>,
}

pub(crate) fn parse_envelope(content: &str) -> Result<Envelope> {
    if content.len() > MAX_MESSAGE_BYTES {
        bail!("peer message exceeds {MAX_MESSAGE_BYTES} bytes");
    }
    if !content.starts_with("<cross-session-message") {
        if serde_json::to_string(content)?.len() > MAX_MESSAGE_BYTES {
            bail!("rendered peer message exceeds context budget");
        }
        return Ok(Envelope {
            body: content.to_owned(),
            ..Default::default()
        });
    }
    let mut reader = Reader::from_str(content);
    let Event::Start(start) = reader.read_event()? else {
        bail!("invalid peer envelope");
    };
    if start.name().as_ref() != b"cross-session-message" {
        bail!("invalid peer envelope tag");
    }
    let body = content[reader.buffer_position() as usize..]
        .strip_prefix('\n')
        .and_then(|text| text.strip_suffix("\n</cross-session-message>"))
        .ok_or_else(|| anyhow::anyhow!("invalid peer envelope boundaries"))?;
    if serde_json::to_string(body)?.len() > MAX_MESSAGE_BYTES {
        bail!("rendered peer message exceeds context budget");
    }
    let mut envelope = Envelope {
        body: body.to_owned(),
        ..Default::default()
    };
    for attribute in start.attributes() {
        let attribute = attribute?;
        let value = attribute
            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())?;
        match attribute.key.as_ref() {
            b"from-mode" => {
                envelope.mode = match value.as_ref() {
                    "bypass" => Some(PermissionMode::Bypass),
                    "prompting" => Some(PermissionMode::Prompting),
                    _ => bail!("invalid peer permission mode"),
                };
            }
            b"hop-chain" => {
                envelope.hop_chain = value.split(',').map(str::to_owned).collect();
                if envelope.hop_chain.len() > 32
                    || envelope.hop_chain.iter().any(|hop| {
                        hop.len() != 24 || !hop.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                {
                    bail!("invalid peer hop chain");
                }
            }
            _ => {}
        }
    }
    Ok(envelope)
}

pub(crate) fn escape_attribute(value: &str) -> String {
    quick_xml::escape::escape(value).into_owned()
}

pub(crate) fn user_frame(
    id: &str,
    address: &str,
    name: &str,
    session_id: &str,
    mode: PermissionMode,
    message: &str,
    hop_chain: &[String],
) -> Result<Value> {
    if message.trim().is_empty() || message.len() > MAX_MESSAGE_BYTES - 1024 {
        bail!(
            "peer text must contain 1..{} bytes",
            MAX_MESSAGE_BYTES - 1024
        );
    }
    let chain = if hop_chain.is_empty() {
        String::new()
    } else {
        format!(" hop-chain=\"{}\"", hop_chain.join(","))
    };
    let body = message
        .replace("<cross-session-message", "<\\cross-session-message")
        .replace("</cross-session-message", "<\\/cross-session-message");
    let content = format!(
        "<cross-session-message from=\"{}\" from-session=\"{}\"{chain} from-name=\"{}\" from-mode=\"{}\">\n{body}\n</cross-session-message>",
        escape_attribute(address),
        escape_attribute(session_id),
        escape_attribute(name),
        mode.as_str(),
    );
    parse_envelope(&content)?;
    Ok(json!({
        "msgV": 1,
        "msg_id": id,
        "type": "user",
        "message": {"role": "user", "content": content},
        "priority": "next",
        "from": address,
    }))
}

pub(crate) fn control_frame(fields: Value) -> Value {
    let mut frame = fields.as_object().cloned().unwrap_or_default();
    frame.insert("type".into(), json!("control"));
    frame.insert("msgV".into(), json!(1));
    frame.insert("msg_id".into(), json!(Uuid::new_v4().to_string()));
    Value::Object(frame)
}

pub(crate) fn admission(
    policy: InboundPolicy,
    receiver: PermissionMode,
    sender: Option<PermissionMode>,
) -> &'static str {
    match policy {
        InboundPolicy::Accept => "pending",
        InboundPolicy::Hold => "held",
        InboundPolicy::Refuse => "refused",
        InboundPolicy::Parity => {
            if sender == Some(receiver) || sender.is_none() && receiver == PermissionMode::Prompting
            {
                "pending"
            } else {
                "held"
            }
        }
    }
}
