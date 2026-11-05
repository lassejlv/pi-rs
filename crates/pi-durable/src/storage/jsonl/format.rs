use crate::{wire, *};
pub(super) const MAIN_FILE: &str = "main.jsonl";
pub(super) const RECLAIM_SUFFIX: &str = ".reclaim";
#[derive(Clone, Debug)]
pub(super) enum MainOperation {
    Direct(StorageWrite),
    Task {
        id: TaskId,
        ordinal: u64,
    },
    Create {
        record: DocumentCreate,
        ordinal: u64,
    },
    Change {
        id: DocumentId,
        ordinal: u64,
    },
}
#[derive(Clone, Debug)]
pub(super) struct MainMarker {
    pub seq: Seq,
    pub writes: Vec<MainOperation>,
}
#[derive(Clone, Debug)]
pub(super) enum SidecarPayload {
    Task(Box<TaskRecord>),
    Document {
        id: DocumentId,
        content: DocumentContent,
    },
}
#[derive(Clone, Debug)]
pub(super) struct SidecarRecord {
    pub seq: Seq,
    pub ordinal: u64,
    pub payload: SidecarPayload,
}
pub(super) fn file_name(kind: &str, id: i64) -> String {
    format!("{kind}-{id}.jsonl")
}
pub(super) fn sidecar_file(name: &str) -> Option<(&str, u64)> {
    let stem = name.strip_suffix(".jsonl")?;
    let (kind, id) = stem.split_once('-')?;
    if !matches!(kind, "doc" | "task")
        || id.is_empty()
        || (id.len() > 1 && id.starts_with('0'))
        || !id.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((kind, id.parse().ok()?))
}
pub(super) fn is_sidecar_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    let Some((kind, id)) = stem.split_once('-') else {
        return false;
    };
    matches!(kind, "doc" | "task")
        && !id.is_empty()
        && (id.len() == 1 || !id.starts_with('0'))
        && id.bytes().all(|b| b.is_ascii_digit())
}
pub(super) fn corrupt(message: impl Into<String>) -> Error {
    Error::jsonl_corruption(message, None)
}
fn parse_json(text: &str, description: &str) -> Result<JsonValue> {
    pi_chord::json::parse(text).map_err(|e| {
        Error::jsonl_corruption(
            format!("Malformed complete {description}"),
            Some(Error::message(e)),
        )
    })
}
fn integer(value: &JsonValue) -> Option<i64> {
    wire::decode::<Seq>(value).ok().map(Seq::get)
}
fn field_integer(value: &JsonValue, field: &str) -> Option<i64> {
    wire::get(value, field).and_then(integer)
}
fn valid_header(value: &JsonValue, tag: &str) -> bool {
    value.is_object()
        && field_integer(value, "format") == Some(1)
        && wire::get(value, "type").is_some_and(|v| v.string_equals(tag))
        && field_integer(value, "seq").is_some_and(|v| v >= 1)
}
fn decode<T: serde::de::DeserializeOwned>(value: &JsonValue, message: &str) -> Result<T> {
    wire::decode(value).map_err(|e| Error::jsonl_corruption(message, Some(Error::message(e))))
}
pub(super) fn parse_marker(text: &str, line: usize) -> Result<MainMarker> {
    let description = format!("{MAIN_FILE} line {line}");
    let value = parse_json(text, &description)?;
    let writes = wire::get(&value, "writes").and_then(JsonValue::as_array);
    if !valid_header(&value, "commit") || writes.is_none() {
        return Err(corrupt(format!("Invalid commit marker in {description}")));
    }
    let writes = writes
        .expect("validated array")
        .iter()
        .map(|v| parse_operation(v, &description))
        .collect::<Result<_>>()?;
    Ok(MainMarker {
        seq: Seq::from_raw(field_integer(&value, "seq").expect("validated sequence")),
        writes,
    })
}
fn parse_operation(value: &JsonValue, description: &str) -> Result<MainOperation> {
    let tag =
        wire::tag(value, "type").map_err(|_| corrupt(format!("Invalid write in {description}")))?;
    let invalid = |what: &str| format!("Invalid {what} in {description}");
    let ordinal = field_integer(value, "ordinal")
        .filter(|v| *v >= 0)
        .map(|v| v as u64);
    let id = field_integer(value, "id");
    let body = wire::get(value, "value");
    match tag.as_str() {
        "conversation" | "entry" | "submission" => {
            let message = invalid(&format!("{tag} write"));
            if body.is_none_or(|v| !v.is_object() || field_integer(v, "id").is_none()) {
                return Err(corrupt(message));
            }
            Ok(MainOperation::Direct(decode(value, &message)?))
        }
        "task" => {
            let message = invalid("terminal task write");
            if body.is_none_or(|v| {
                !v.is_object()
                    || field_integer(v, "id").is_none()
                    || wire::get(v, "state")
                        .and_then(|s| wire::get(s, "status"))
                        .is_none_or(|s| !s.string_equals("terminal"))
            }) {
                return Err(corrupt(message));
            }
            Ok(MainOperation::Direct(decode(value, &message)?))
        }
        "document.retire" => Ok(MainOperation::Direct(StorageWrite::DocumentRetire {
            id: DocumentId::from_raw(id.ok_or_else(|| corrupt(invalid("document retirement")))?),
        })),
        "task.sidecar" => {
            let message = invalid("task sidecar write");
            Ok(MainOperation::Task {
                id: TaskId::from_raw(id.ok_or_else(|| corrupt(&message))?),
                ordinal: ordinal.ok_or_else(|| corrupt(message))?,
            })
        }
        "document.create" => {
            let message = invalid("document creation");
            let record = wire::get(value, "record")
                .filter(|v| v.is_object() && field_integer(v, "id").is_some())
                .ok_or_else(|| corrupt(&message))?;
            Ok(MainOperation::Create {
                record: decode(record, &message)?,
                ordinal: ordinal.ok_or_else(|| corrupt(message))?,
            })
        }
        "document.change" => {
            let message = invalid("document change");
            Ok(MainOperation::Change {
                id: DocumentId::from_raw(id.ok_or_else(|| corrupt(&message))?),
                ordinal: ordinal.ok_or_else(|| corrupt(message))?,
            })
        }
        _ => Err(corrupt(format!("Unknown write type in {description}"))),
    }
}
fn parse_content(value: &JsonValue, description: &str) -> Result<DocumentContent> {
    let message = format!("Invalid document content in {description}");
    if !value.is_object() || !field_integer(value, "version").is_some_and(|v| v >= 1) {
        return Err(corrupt(message));
    }
    let version = field_integer(value, "version").expect("validated version") as u64;
    if wire::get(value, "kind").is_some_and(|v| v.string_equals("base")) {
        let body = wire::get(value, "value")
            .filter(|v| v.is_object())
            .ok_or_else(|| corrupt(&message))?;
        Ok(DocumentContent::Base {
            version,
            value: decode(body, &message)?,
        })
    } else if wire::get(value, "kind").is_some_and(|v| v.string_equals("delta")) {
        let ops = wire::get(value, "ops")
            .filter(|v| v.is_array())
            .ok_or_else(|| corrupt(&message))?;
        Ok(DocumentContent::Delta {
            version,
            ops: decode(ops, &message)?,
        })
    } else {
        Err(corrupt(message))
    }
}
pub(super) fn parse_record(text: &str, file: &str, line: usize) -> Result<SidecarRecord> {
    let description = format!("{file} line {line}");
    let value = parse_json(text, &description)?;
    let ordinal = field_integer(&value, "ordinal").filter(|n| *n >= 0);
    let payload = wire::get(&value, "payload");
    if !valid_header(&value, "record")
        || ordinal.is_none()
        || payload.is_none_or(|v| !v.is_object() || wire::tag(v, "type").is_err())
    {
        return Err(corrupt(format!("Invalid sidecar record in {description}")));
    }
    let payload = payload.expect("validated payload");
    let payload = match wire::tag(payload, "type").expect("validated tag").as_str() {
        "task" => {
            let message = format!("Invalid live task record in {description}");
            let record = wire::get(payload, "value").ok_or_else(|| corrupt(&message))?;
            if !record.is_object()
                || field_integer(record, "id").is_none()
                || wire::get(record, "state").is_none_or(|s| {
                    !s.is_object()
                        || wire::get(s, "status").is_some_and(|v| v.string_equals("terminal"))
                })
            {
                return Err(corrupt(message));
            }
            SidecarPayload::Task(decode(record, &message)?)
        }
        "document" => {
            let id = field_integer(payload, "id")
                .ok_or_else(|| corrupt(format!("Invalid document record in {description}")))?;
            let content = wire::get(payload, "content")
                .ok_or_else(|| corrupt(format!("Invalid document content in {description}")))?;
            SidecarPayload::Document {
                id: DocumentId::from_raw(id),
                content: parse_content(content, &description)?,
            }
        }
        _ => {
            return Err(corrupt(format!(
                "Unknown sidecar record type in {description}"
            )));
        }
    };
    Ok(SidecarRecord {
        seq: Seq::from_raw(field_integer(&value, "seq").expect("validated sequence")),
        ordinal: ordinal.expect("validated ordinal") as u64,
        payload,
    })
}
impl MainOperation {
    fn json(&self) -> JsonValue {
        let mut value = wire::object();
        match self {
            Self::Direct(write) => return wire::encode(write),
            Self::Task { id, ordinal } => {
                wire::put(&mut value, "type", "task.sidecar");
                wire::put(&mut value, "id", id);
                wire::put(&mut value, "ordinal", ordinal);
            }
            Self::Create { record, ordinal } => {
                wire::put(&mut value, "type", "document.create");
                wire::put(&mut value, "record", record);
                wire::put(&mut value, "ordinal", ordinal);
            }
            Self::Change { id, ordinal } => {
                wire::put(&mut value, "type", "document.change");
                wire::put(&mut value, "id", id);
                wire::put(&mut value, "ordinal", ordinal);
            }
        }
        value
    }
}
fn header(kind: &str, seq: Seq) -> JsonValue {
    let mut value = wire::object();
    wire::put(&mut value, "format", &1);
    wire::put(&mut value, "type", kind);
    wire::put(&mut value, "seq", &seq);
    value
}
fn json_line(value: &JsonValue) -> String {
    let mut line = pi_chord::json::stringify(value);
    line.push('\n');
    line
}
impl MainMarker {
    pub fn line(&self) -> String {
        let mut v = header("commit", self.seq);
        wire::put(
            &mut v,
            "writes",
            &self
                .writes
                .iter()
                .map(MainOperation::json)
                .collect::<Vec<_>>(),
        );
        json_line(&v)
    }
}
impl SidecarRecord {
    pub fn line(&self) -> String {
        let mut v = header("record", self.seq);
        wire::put(&mut v, "ordinal", &self.ordinal);
        let mut payload = wire::object();
        match &self.payload {
            SidecarPayload::Task(record) => {
                wire::put(&mut payload, "type", "task");
                wire::put(&mut payload, "value", record);
            }
            SidecarPayload::Document { id, content } => {
                wire::put(&mut payload, "type", "document");
                wire::put(&mut payload, "id", id);
                wire::put(&mut payload, "content", content);
            }
        }
        wire::put(&mut v, "payload", &payload);
        json_line(&v)
    }
}
