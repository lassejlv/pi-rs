//! Original renderers remain host capabilities; rendering policy and planning run in Rust.
use super::*;
use pi_durable::harness::{
    context::ContextView,
    declaration::ToolDeclaration,
    prompt::{self, PromptSection, Sections},
    registry::RegisteredSection,
};

#[derive(Default)]
pub(super) struct PromptBridge {
    pub replays: AtomicU64,
    pub plans: AtomicU64,
    pub renders: AtomicU64,
    pub section_calls: AtomicU64,
}
struct HostSection {
    id: u64,
    key: JsonString,
    tagged: bool,
    hub: Arc<Hub>,
}
impl RegisteredSection for HostSection {
    fn key(&self) -> &JsonString {
        &self.key
    }
}
impl PromptSection<u64> for HostSection {
    fn tagged(&self) -> bool {
        self.tagged
    }
    fn render(&self, input: Arc<u64>, context: &Context) -> StorageFuture<Option<JsonString>> {
        self.hub
            .prompt
            .section_calls
            .fetch_add(1, Ordering::Relaxed);
        let call = self.hub.call_context(
            "promptRender",
            object([("section", encode(&self.id)), ("input", encode(&*input))]),
            context,
        );
        Box::pin(async move { call.await?.as_ref().map(decode).transpose() })
    }
}
impl Hub {
    pub(super) fn prompt_request(
        self: Arc<Self>,
        method: &str,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        let value = match method {
            "prompt.replay" => {
                self.prompt.replays.fetch_add(1, Ordering::Relaxed);
                encode_pairs(&prompt::replay_sections(&field::<Vec<Arc<JsonValue>>>(
                    &request, "messages",
                )?)?)
            }
            "prompt.plan" => {
                self.prompt.plans.fetch_add(1, Ordering::Relaxed);
                let view = &request["view"];
                let view = ContextView {
                    head: field(view, "head")?,
                    entries: field(view, "entries")?,
                    messages: field(view, "messages")?,
                    contributions: Vec::new(),
                };
                let drafts = prompt::plan_system_entries(
                    &view,
                    &pairs(&request, "desired")?,
                    &field::<Vec<ToolDeclaration>>(&request, "tools")?,
                    field(&request, "timestamp")?,
                )?;
                let mut drafts = encode(&drafts);
                for draft in drafts.as_array_mut().unwrap() {
                    draft
                        .as_object_mut()
                        .unwrap()
                        .shift_remove(&JsonString::from("kind"));
                }
                drafts
            }
            "prompt.render" => {
                self.prompt.renders.fetch_add(1, Ordering::Relaxed);
                let sections = field::<Vec<JsonValue>>(&request, "sections")?
                    .iter()
                    .map(|section| {
                        Ok(HostSection {
                            id: field(section, "id")?,
                            key: field(section, "key")?,
                            tagged: field(section, "tagged")?,
                            hub: self.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let input = Arc::new(field::<u64>(&request, "input")?);
                let shown = pairs(&request, "shown")?;
                let report: u64 = field(&request, "report")?;
                return Ok(Box::pin(async move {
                    let rendered = prompt::render_sections(
                        &sections,
                        input,
                        &shown,
                        |error, section| {
                            self.sync(
                                "promptReport",
                                object([
                                    ("report", encode(&report)),
                                    ("section", encode(&section.id)),
                                    ("error", error_value(&error)),
                                ]),
                            )?;
                            Ok(())
                        },
                        &context,
                    )
                    .await?;
                    Ok(Some(encode_pairs(&rendered)))
                }));
            }
            other => return Err(Error::message(format!("Unknown prompt method {other}"))),
        };
        Ok(Box::pin(async move { Ok(Some(value)) }))
    }
}
fn pairs(request: &JsonValue, key: &str) -> Result<Sections> {
    Ok(field::<Vec<(JsonString, JsonString)>>(request, key)?
        .into_iter()
        .collect())
}
fn encode_pairs(sections: &Sections) -> JsonValue {
    encode(&sections.iter().collect::<Vec<_>>())
}
