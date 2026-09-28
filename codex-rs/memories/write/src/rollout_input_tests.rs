use super::*;
use pretty_assertions::assert_eq;

#[test]
fn extraction_chunks_preserve_unicode_evidence_with_bounded_messages() {
    let evidence = "User correction: 🐈\n".repeat(2_000);
    let mut reconstructed = String::new();
    for message in extraction_messages(&evidence) {
        let ResponseItem::Message { role, content, .. } = message else {
            panic!("message")
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("text")
        };
        assert!(text.len() < 9_000);
        reconstructed.push_str(text);
    }
    assert_eq!(reconstructed, evidence);
}

#[test]
fn classifies_memory_excluded_fragments() {
    let cases = [
        (
            "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
            true,
        ),
        (
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
            true,
        ),
        (
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
            true,
        ),
        (
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
            false,
        ),
        (
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
            false,
        ),
    ];

    for (text, expected) in cases {
        assert_eq!(
            is_memory_excluded_contextual_user_fragment(&ContentItem::InputText {
                text: text.to_string(),
            }),
            expected,
            "{text}",
        );
    }
}

#[test]
fn panng_memory_input_cannot_silently_discard_atomic_runtime_records() {
    let commit = codex_rollout::PanngCommit::from_slice(
        br#"{
        "version":1,"branch":"00000000-0000-4000-8000-000000000001",
        "commit_id":"00000000-0000-4000-8000-000000000002","sequence":1,"previous_sequence":0,
        "mutation":{"Delta":{"changes":[]}},"host_effects":[],"checkpoint":null
    }"#,
    )
    .unwrap();
    let items = vec![RolloutItem::PanngCommit(Box::new(commit))];
    assert!(serialize_tiered_input(&items, /*token_limit*/ 1000).is_err());
}
