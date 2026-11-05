use pi_durable::{documents::*, *};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct State {
    value: i64,
}
impl DocumentState for State {}
fn initial() -> Result<State> {
    Ok(State { value: 0 })
}
fn value(source: &str) -> JsonObject {
    serde_json::from_str(source).unwrap()
}

#[test]
fn definitions_require_a_checkpoint_policy_and_validate_safe_positive_versions() {
    for version in [0, MAX_SAFE_INTEGER as u64 + 1] {
        assert!(
            define_doc(DocDefinition::new(
                "kind",
                version,
                SessionScope,
                initial,
                |_, _, _| false
            ))
            .err()
            .unwrap()
            .to_string()
            .contains("positive integer")
        );
    }
    let token = define_doc(DocDefinition::new(
        "",
        MAX_SAFE_INTEGER as u64,
        SessionScope,
        initial,
        |_, _, _| false,
    ))
    .unwrap();
    assert!(token.definition().kind().units().is_empty());
    assert_eq!(token.definition().version(), MAX_SAFE_INTEGER as u64);
    assert_eq!(
        token.definition().initial(None).unwrap(),
        value(r#"{"value":0}"#)
    );
}

#[test]
fn owner_and_family_addresses_match_the_reference_identity_format() {
    let session = define_doc(DocDefinition::new(
        "t.session",
        1,
        SessionScope,
        initial,
        |_, _, _| false,
    ))
    .unwrap();
    let task = define_doc(DocDefinition::new(
        "t.task",
        1,
        TaskScope,
        initial,
        |_, _, _| false,
    ))
    .unwrap();
    let conversation = define_doc(DocDefinition::new(
        "t.conversation",
        1,
        RewindableConversationScope {
            fork: ForkPolicy::AsOf,
        },
        initial,
        |_, _, _| false,
    ))
    .unwrap();
    assert_eq!(
        address_id(&session.address(())),
        r#"["t.session","session",null,null]"#
    );
    assert_eq!(
        address_id(&task.address(TaskId::from_raw(3))),
        r#"["t.task","task",3,null]"#
    );
    assert_eq!(
        address_id(&conversation.address(ROOT_CONVERSATION_ID)),
        r#"["t.conversation","conversation",1,null]"#
    );
    let family = define_doc_family(DocFamilyDefinition::new(
        "family",
        1,
        SessionScope,
        |seed: i64| Ok(State { value: seed }),
        |_, _, _| false,
    ))
    .unwrap();
    assert_eq!(
        family
            .definition()
            .initial(Some(&JsonValue::from(7usize)))
            .unwrap(),
        value(r#"{"value":7}"#)
    );
    assert_eq!(
        address_id(&family.address((), "key")),
        r#"["family","session",null,"key"]"#
    );
    assert!(family.definition().initial(None).is_err());
    let key = JsonString::from_utf16(vec![0xd800]);
    assert_eq!(
        address_id(&family.address((), key)),
        r#"["family","session",null,"\ud800"]"#
    );
}

#[test]
fn untyped_argument_resolution_retains_owner_key_and_next_argument() {
    let family = define_doc_family(DocFamilyDefinition::new(
        "family",
        1,
        TaskScope,
        |seed: i64| Ok(State { value: seed }),
        |_, _, _| false,
    ))
    .unwrap();
    let args: Vec<JsonValue> = serde_json::from_str(r#"[3,"key",7]"#).unwrap();
    let resolved = resolve_address(family.definition(), &args).unwrap();
    assert_eq!(resolved.next_argument, 2);
    assert_eq!(resolved.address, family.address(TaskId::from_raw(3), "key"));
    assert_eq!(resolved.id, r#"["family","task",3,"key"]"#);
    for args in [vec![], vec![JsonValue::Null], vec![JsonValue::from("bad")]] {
        assert_eq!(
            resolve_address(family.definition(), &args)
                .err()
                .unwrap()
                .to_string(),
            "Document family requires a task ID"
        );
    }
}

#[test]
fn incarnation_creation_and_semantics_checks_preserve_history_and_fork_policy() {
    let rewindable = define_doc(DocDefinition::new(
        "notes",
        1,
        RewindableConversationScope {
            fork: ForkPolicy::AsOf,
        },
        initial,
        |_, _, _| false,
    ))
    .unwrap();
    let latest = define_doc(DocDefinition::new(
        "notes",
        1,
        LatestConversationScope {
            fork: LatestFork::Current,
        },
        initial,
        |_, _, _| false,
    ))
    .unwrap();
    let record = rewindable
        .definition()
        .create(
            &rewindable.address(ROOT_CONVERSATION_ID),
            DocumentId::from_raw(7),
        )
        .unwrap();
    assert_eq!(
        record.semantics,
        DocumentSemantics::Conversation {
            conversation_id: ROOT_CONVERSATION_ID,
            history: ConversationHistory::Rewindable {
                fork: ForkPolicy::AsOf
            }
        }
    );
    rewindable.definition().check_record_scope(&record).unwrap();
    assert_eq!(
        latest
            .definition()
            .check_record_scope(&record)
            .unwrap_err()
            .to_string(),
        "Document 7 (notes) does not match the supplied definition semantics"
    );
}

#[test]
fn materialization_migrates_only_older_values_and_rejects_unknown_newer_versions() {
    let token = define_doc(
        DocDefinition::new("settings", 2, SessionScope, initial, |_, _, info| {
            info.deltas_since_base >= 2
        })
        .with_migration(|old, from| {
            assert_eq!(from, 1);
            assert_eq!(old, value(r#"{"old":3}"#));
            Ok(State { value: 3 })
        }),
    )
    .unwrap();
    let record = token
        .definition()
        .create(&token.address(()), DocumentId::from_raw(9))
        .unwrap();
    assert_eq!(
        token
            .definition()
            .materialize_value(&record, 1, value(r#"{"old":3}"#))
            .unwrap(),
        value(r#"{"value":3}"#)
    );
    assert_eq!(
        token
            .definition()
            .materialize_value(&record, 2, value(r#"{"value":8}"#))
            .unwrap(),
        value(r#"{"value":8}"#)
    );
    assert_eq!(
        token
            .definition()
            .check_record_version(&record, 3)
            .unwrap_err()
            .to_string(),
        "Document 9 (settings) has newer version 3 than 2"
    );
    let no_migration = define_doc(DocDefinition::new(
        "settings",
        2,
        SessionScope,
        initial,
        |_, _, _| false,
    ))
    .unwrap();
    assert_eq!(
        no_migration
            .definition()
            .check_record_version(&record, 1)
            .unwrap_err()
            .to_string(),
        "Document 9 (settings) requires migration from version 1"
    );
    assert!(
        token
            .definition()
            .checkpoint_when(
                &value(r#"{"value":8}"#),
                &[],
                CheckpointInfo {
                    deltas_since_base: 2
                }
            )
            .unwrap()
    );
    assert!(
        !token
            .definition()
            .checkpoint_when(
                &value(r#"{"value":8}"#),
                &[],
                CheckpointInfo {
                    deltas_since_base: 1
                }
            )
            .unwrap()
    );
}

#[test]
fn serialized_document_versions_preserve_the_full_safe_integer_range() {
    let record: DocumentContent =
        serde_json::from_str(r#"{"kind":"base","version":9007199254740991,"value":{}}"#).unwrap();
    assert_eq!(record.version(), MAX_SAFE_INTEGER as u64);
}
