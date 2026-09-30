use super::*;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;

#[test]
fn frontend_admission_keeps_permission_classes_and_clears_on_drop() {
    let runtime = FrontendPeerRuntime::default();
    assert!(!runtime.permits_mode(PermissionMode::Prompting));
    {
        *runtime.admission.lock().unwrap() = Some((
            Some(PermissionMode::Prompting),
            false,
            InboundPolicy::Parity,
        ));
        let _reset = FrontendAdmissionReset(&runtime);
        assert!(runtime.permits_mode(PermissionMode::Prompting));
        assert!(!runtime.permits_mode(PermissionMode::Bypass));
    }
    assert!(!runtime.permits_mode(PermissionMode::Prompting));
    *runtime.admission.lock().unwrap() = Some((None, false, InboundPolicy::Parity));
    assert!(runtime.permits_mode(PermissionMode::Prompting));
    assert!(!runtime.permits_mode(PermissionMode::Bypass));
    *runtime.admission.lock().unwrap() =
        Some((Some(PermissionMode::Bypass), false, InboundPolicy::Refuse));
    assert!(!runtime.permits_mode(PermissionMode::Bypass));
    *runtime.admission.lock().unwrap() =
        Some((Some(PermissionMode::Prompting), true, InboundPolicy::Parity));
    assert!(runtime.permits_mode(PermissionMode::Bypass));
}

#[test]
fn frontend_persistence_confirmation_only_recognizes_its_external_tool_envelope() {
    let message = codex_claude_peer::ReceivedMessage {
        id: "message-id".into(),
        sender: "uds:/tmp/cc-socks/sender.sock".into(),
        sender_session: "sender-session".into(),
        sender_name: "sender".into(),
        sender_pid: 123,
        sender_start: "test-start".into(),
        body: "test body".into(),
        mode: Some(PermissionMode::Prompting),
        hop_chain: Vec::new(),
        approval_released: false,
    };
    let item = |namespace: &str, call_id: Option<String>| ResponseItem::FunctionCallOutput {
        id: None,
        call_id,
        name: Some("received_message".into()),
        namespace: Some(namespace.into()),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(serde_json::to_string(&message).unwrap()),
            success: None,
        },
        internal_chat_message_metadata_passthrough: None,
    };
    assert_eq!(
        external_message(&item("cross_session", None)),
        Some(message.clone())
    );
    assert!(external_message(&item("another_namespace", None)).is_none());
    assert!(external_message(&item("cross_session", Some("ordinary-tool-call".into()))).is_none());
}

#[test]
fn frontend_permanent_rejections_are_not_retryable() {
    assert!(!ExternalPeerError::Unavailable.retryable());
    assert!(!ExternalPeerError::NotAdmitted.retryable());
    assert!(ExternalPeerError::Temporary(anyhow::anyhow!("temporary storage failure")).retryable());
}
