//! Default policies and entry kinds equal upstream's (printed by running `harness/agent.ts` and
//! `entries.ts` of pi-mono 5e63241 with `--conditions=source`).
use pi_durable::{harness::agent::INSTRUCTIONS_KEY, *};

fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}

#[test]
fn default_retry_compaction_and_progress_policies_equal_upstream() {
    assert_eq!(
        DEFAULT_RETRY_POLICY.value(),
        &json(r#"{"enabled":true,"maxRetries":3,"baseDelayMs":2000,"maxAgentDelayMs":60000}"#)
    );
    assert_eq!(
        DEFAULT_COMPACTION_POLICY.value(),
        &json(
            r#"{"enabled":true,"reserveTokens":16384,"keepRecentTokens":20000,"backgroundTokens":32768}"#
        )
    );
    assert_eq!(DEFAULT_PROGRESS_POLICY.partial_interval_ms, 100.0);
    assert_eq!(DEFAULT_PROGRESS_POLICY.output_interval_ms, 100.0);
    assert_eq!(DEFAULT_RESTART_LIMIT, 5.0);
    assert_eq!(INSTRUCTIONS_KEY, "instructions");
}

#[test]
fn entry_constants_carry_upstreams_kinds() {
    let kinds = [
        (&*USER_ENTRY, "pi.user"),
        (&*ASSISTANT_ENTRY, "pi.assistant"),
        (&*SYSTEM_ENTRY, "pi.system"),
        (&*TOOL_RESULT_ENTRY, "pi.tool-result"),
        (&*RESET_ENTRY, "pi.reset"),
        (&*COMPACTION_ENTRY, "pi.compaction"),
    ];
    for (entry, kind) in kinds {
        assert_eq!(entry.kind().to_string_lossy(), kind);
        assert_eq!(*entry.kind(), JsonString::from(kind));
    }
    assert!(define_entry::<JsonValue>("").is_err());
}
