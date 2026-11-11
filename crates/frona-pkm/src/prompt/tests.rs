use super::*;

#[test]
fn aliases_are_short_stable_and_expand_only_known_values() {
    let ids = PromptIds::new("m", ["uuid-a".into(), "uuid-b".into(), "uuid-a".into()]);
    assert_eq!(ids.local("uuid-a"), "m1");
    assert_eq!(ids.local("uuid-b"), "m2");
    assert_eq!(ids.expand("m2").unwrap(), "uuid-b");
    assert!(ids.expand("m3").is_err());
}

#[test]
fn prompt_evidence_keeps_quote_but_hides_infrastructure_ids() {
    let evidence = vec![MemoryEvidence {
        strength: crate::model::EvidenceStrength::Explicit,
        source: EvidenceSource::AgentMessage {
            message_id: "019-message-uuid".into(),
            agent_id: "019-agent-uuid".into(),
            chat_id: "019-chat-uuid".into(),
            quote: "Casey Owner works at Example Corp".into(),
        },
    }];
    let rendered = prompt_evidence(&evidence);
    assert!(rendered.contains("Casey Owner works at Example Corp"));
    assert!(!rendered.contains("019-"));
}
