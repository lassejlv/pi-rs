//! Original stock submission handles delegate every operation to the native service.
use super::*;
#[derive(Default)]
pub(super) struct SubmissionBridge {
    pub services: Mutex<HashMap<u64, Submissions>>,
    pub admissions: AtomicU64,
    pub reads: AtomicU64,
    pub waits: AtomicU64,
    pub aborts: AtomicU64,
}
impl Hub {
    pub(super) fn submission_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        let harness: u64 = field(&request, "harness")?;
        let service = self
            .submissions
            .services
            .lock()
            .get(&harness)
            .cloned()
            .ok_or_else(|| Error::message("Unknown submission service"))?;
        Ok(match method {
            "submission.submit" => {
                self.submissions.admissions.fetch_add(1, Ordering::Relaxed);
                let operation = service.submit(
                    field(&request, "conversation")?,
                    decode_draft(&request["draft"])?,
                    &context,
                );
                Box::pin(async move { Ok(Some(encode(&operation.await?.id))) })
            }
            "submission.get" => {
                self.submissions.reads.fetch_add(1, Ordering::Relaxed);
                let operation = service.get(field(&request, "submission")?, &context);
                Box::pin(
                    async move { Ok(operation.await?.map(|submission| encode(&submission.id))) },
                )
            }
            "submission.status" => {
                self.submissions.reads.fetch_add(1, Ordering::Relaxed);
                let operation = service.status(field(&request, "submission")?, &context);
                Box::pin(async move { Ok(Some(encode(&operation.await?))) })
            }
            "submission.wait" => {
                self.submissions.waits.fetch_add(1, Ordering::Relaxed);
                let operation = service.wait(field(&request, "submission")?, &context);
                Box::pin(async move { Ok(Some(encode(&operation.await?))) })
            }
            "submission.abort" => {
                self.submissions.aborts.fetch_add(1, Ordering::Relaxed);
                let id = field(&request, "submission")?;
                let operation = service.abort(id, &context, field(&request, "conversation")?);
                let handle = request["handle"].as_bool() == Some(true);
                Box::pin(async move {
                    Ok(Some(match operation.await? {
                        Some(result) => encode(&result),
                        None if handle => {
                            return Err(Error::message(format!("Submission {id} does not exist")));
                        }
                        None => encode(&"not_found"),
                    }))
                })
            }
            other => {
                return Err(Error::message(format!(
                    "Unknown submission operation {other}"
                )));
            }
        })
    }
}

// Untyped host inputs are checked before deserializing the native typed draft.
// A number in entry.kind must produce the source TypeError, not a serde string error.
pub(super) fn decode_draft(value: &JsonValue) -> Result<SubmissionDraft> {
    if value["type"].string_equals("input")
        && let Some(entry) = value["entry"].as_object()
    {
        let kind = entry.get(&JsonString::from("kind"));
        if !kind.is_some_and(JsonValue::is_string) {
            let mut units =
                "An input entry's kind must be a non-empty string not starting with \"pi.\", got "
                    .encode_utf16()
                    .collect::<Vec<_>>();
            units.extend(host_string(kind).units());
            return Err(Error::host("TypeError", JsonString::from_utf16(units)));
        }
    }
    decode(value)
}
fn host_string(value: Option<&JsonValue>) -> JsonString {
    match value {
        None => "undefined".into(),
        Some(JsonValue::String(value)) => value.clone(),
        Some(JsonValue::Object(_)) => "[object Object]".into(),
        Some(JsonValue::Array(values)) => {
            let mut units = Vec::new();
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    units.push(b',' as u16);
                }
                if !value.is_null() {
                    units.extend(host_string(Some(value)).units());
                }
            }
            JsonString::from_utf16(units)
        }
        Some(value) => value.to_string().into(),
    }
}
