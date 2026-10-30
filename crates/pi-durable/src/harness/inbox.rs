//! Durable queue boundaries. Table reads happen before any entry/submission write.
use super::types::*;
use crate::{wire, *};
use pi_chord::delta::{Op, Seg};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub enum InboxItem {
    Input {
        id: SubmissionId,
        mode: InputMode,
        content: UserInput,
        entry: Option<InputEntry>,
    },
    Write {
        id: SubmissionId,
        entry: EntryDraft,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
}
impl InboxItem {
    pub fn id(&self) -> SubmissionId {
        match self {
            Self::Input { id, .. } | Self::Write { id, .. } => *id,
        }
    }
    fn to_json(&self) -> JsonValue {
        let mut value = wire::object();
        wire::put(&mut value, "id", &self.id());
        match self {
            Self::Input {
                mode,
                content,
                entry,
                ..
            } => {
                wire::put(&mut value, "mode", mode);
                wire::put(&mut value, "content", content);
                wire::put_optional(&mut value, "entry", entry);
            }
            Self::Write { entry, .. } => {
                wire::put(&mut value, "mode", &"write");
                wire::put(&mut value, "entry", entry);
            }
        }
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        let id = wire::field(value, "id")?;
        Ok(if wire::tag(value, "mode")? == "write" {
            Self::Write {
                id,
                entry: wire::field(value, "entry")?,
            }
        } else {
            Self::Input {
                id,
                mode: wire::field(value, "mode")?,
                content: wire::field(value, "content")?,
                entry: wire::optional(value, "entry")?,
            }
        })
    }
}
wire::raw_serde!(InboxItem);
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InboxState {
    pub items: Vec<InboxItem>,
}
impl DocumentState for InboxState {}
pub static INBOX_DOC: std::sync::LazyLock<LatestConversationDocToken<InboxState>> =
    std::sync::LazyLock::new(|| {
        define_doc(DocDefinition::new(
            "pi.inbox",
            1,
            LatestConversationScope {
                fork: LatestFork::Initial,
            },
            || Ok(InboxState::default()),
            |value: &InboxState, _, _| value.items.is_empty(),
        ))
        .expect("valid built-in inbox definition")
    });
pub fn inbox_doc() -> LatestConversationDocToken<InboxState> {
    INBOX_DOC.clone()
}
pub struct Boundary {
    pub conversation_id: ConversationId,
    pub inbox: DocDraft<InboxState>,
    pub modes: QueueModes,
    pub head: Option<EntryId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryAt {
    PostTools,
    Final,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryResult {
    pub users: Vec<SubmissionId>,
    pub reset: bool,
}
pub async fn prepare_boundary(
    tx: &Transaction,
    conversation_id: ConversationId,
    modes: QueueModes,
) -> Result<Boundary> {
    let head = tx
        .latest_head_marker(conversation_id)
        .await?
        .and_then(|entry| entry.head);
    let inbox = tx.doc(&inbox_doc(), conversation_id).await?;
    Ok(Boundary {
        conversation_id,
        inbox,
        modes,
        head,
    })
}
pub async fn apply_boundary(
    tx: &Transaction,
    boundary: &mut Boundary,
    at: BoundaryAt,
    now: f64,
) -> Result<BoundaryResult> {
    let state = boundary.inbox.read()?;
    let reset = has_queued_reset(&state.items);
    let final_boundary = at == BoundaryAt::Final || reset;
    let pick = |mode, queue_mode| {
        let mut indexes = state
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                matches!(item,InboxItem::Input {mode:input_mode,..} if *input_mode==mode)
                    .then_some(i)
            })
            .collect::<Vec<_>>();
        if queue_mode != QueueMode::All {
            indexes.truncate(1);
        }
        indexes
    };
    let writes = state
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| matches!(item, InboxItem::Write { .. }).then_some(i))
        .collect::<Vec<_>>();
    let mut users = pick(InputMode::Steer, boundary.modes.steering_mode);
    if final_boundary {
        users.extend(pick(InputMode::FollowUp, boundary.modes.follow_up_mode));
    }
    users.sort_unstable();
    for index in &writes {
        let InboxItem::Write { id, entry: draft } = &state.items[*index] else {
            unreachable!()
        };
        if is_stale(boundary, draft) {
            tx.settle_submission(
                *id,
                SubmissionSettlement::Unanswered {
                    reason: "stale".into(),
                    detail: None,
                },
            )?;
            continue;
        }
        let entry = tx
            .append_entry(boundary.conversation_id, draft.clone())
            .await?;
        if let Some(head) = draft.head {
            boundary.head = Some(match head {
                EntryHead::Entry(id) => id,
                EntryHead::ThisEntry => entry.id,
            });
        }
        tx.place_submission(*id, entry.id)?;
    }
    let mut placed = Vec::new();
    for index in &users {
        let InboxItem::Input {
            id, content, entry, ..
        } = &state.items[*index]
        else {
            unreachable!()
        };
        let entry = append_input(
            tx,
            boundary.conversation_id,
            content.clone(),
            entry.as_ref(),
            now,
        )
        .await?;
        tx.place_submission(*id, entry.id)?;
        placed.push(*id);
    }
    let mut removed = writes;
    removed.extend(users);
    removed.sort_unstable_by(|a, b| b.cmp(a));
    let ops = removed
        .into_iter()
        .map(|index| Op::Splice(vec![Seg::from("items")], index, 1, vec![]))
        .collect::<Vec<_>>();
    boundary.inbox.apply_ops(&ops)?;
    Ok(BoundaryResult {
        users: placed,
        reset,
    })
}
pub async fn append_input(
    tx: &Transaction,
    conversation_id: ConversationId,
    content: UserInput,
    entry: Option<&InputEntry>,
    now: f64,
) -> Result<EntryRecord> {
    let mut draft = EntryDraft::new(
        entry.map_or_else(|| USER_ENTRY.kind().clone(), |entry| entry.kind.clone()),
    );
    draft.data = entry.and_then(|entry| entry.data.clone());
    let mut message = wire::object();
    wire::put(&mut message, "role", &"user");
    wire::put(&mut message, "content", &content);
    wire::put(&mut message, "timestamp", &now);
    draft.model = Some(vec![message]);
    tx.append_entry(conversation_id, draft).await
}
pub fn is_display_only(entry: &EntryDraft) -> bool {
    entry.model.is_none() && entry.head.is_none() && entry.edits.is_none()
}
pub fn has_queued_reset(items: &[InboxItem]) -> bool {
    items.iter().any(|item| matches!(item,InboxItem::Write {entry,..} if matches!(entry.head,Some(EntryHead::ThisEntry))))
}
pub fn is_stale(boundary: &Boundary, entry: &EntryDraft) -> bool {
    matches!((entry.head,boundary.head),(Some(EntryHead::Entry(head)),Some(active)) if head<active)
}
pub fn push_inbox_item(inbox: &DocDraft<InboxState>, item: InboxItem) -> Result<()> {
    let index = inbox.read()?.items.len();
    inbox.apply_ops(&[Op::Splice(
        vec![Seg::from("items")],
        index,
        0,
        vec![wire::encode(&item)],
    )])
}
pub async fn remove_inbox_item(
    tx: &Transaction,
    conversation_id: ConversationId,
    id: SubmissionId,
) -> Result<()> {
    let inbox = tx.doc(&inbox_doc(), conversation_id).await?;
    if let Some(index) = inbox.read()?.items.iter().position(|item| item.id() == id) {
        inbox.apply_ops(&[Op::Splice(vec![Seg::from("items")], index, 1, vec![])])?;
    }
    Ok(())
}
pub async fn withdraw_queued_inputs(
    tx: &Transaction,
    conversation_id: ConversationId,
    reason: impl Into<JsonString>,
) -> Result<()> {
    let reason = reason.into();
    let inbox = tx.doc(&inbox_doc(), conversation_id).await?;
    let state = inbox.read()?;
    for (index, item) in state.items.iter().enumerate().rev() {
        if matches!(item, InboxItem::Write { .. }) {
            continue;
        }
        tx.settle_submission(
            item.id(),
            SubmissionSettlement::Unanswered {
                reason: reason.clone(),
                detail: None,
            },
        )?;
        inbox.apply_ops(&[Op::Splice(vec![Seg::from("items")], index, 1, vec![])])?;
    }
    Ok(())
}
