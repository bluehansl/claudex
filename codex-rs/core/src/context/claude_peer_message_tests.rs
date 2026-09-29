use super::*;
use pretty_assertions::assert_eq;

#[test]
fn peer_context_matches_assistant_api_contract_without_user_authority() {
    let message = ClaudePeerMessage(ReceivedMessage {
        id: "message-1".into(),
        sender: "uds:/tmp/cc-socks/123.sock".into(),
        sender_session: "session-1".into(),
        sender_name: "Claude".into(),
        sender_pid: 123,
        sender_start: "start".into(),
        body: "Please reply ACK".into(),
        mode: None,
        hop_chain: Vec::new(),
        approval_released: false,
    });
    let expected = message.render();
    let item = ContextualUserFragment::into(message);
    assert!(!crate::context::is_user_authorization_message(&item));
    let codex_protocol::models::ResponseItem::Message { role, content, .. } = item else {
        panic!("message expected");
    };
    assert_eq!(
        (role, content),
        (
            "assistant".into(),
            vec![ContentItem::OutputText { text: expected }]
        )
    );
}
