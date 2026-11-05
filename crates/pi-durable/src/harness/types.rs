//! Host submissions retain lossless provider content until model conversion.
use crate::{wire, *};
use serde::{Deserialize, Serialize};

pub type UserInput = JsonValue;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: JsonString,
    pub model_id: JsonString,
}

#[derive(Clone, Debug)]
pub struct InputEntry {
    pub kind: JsonString,
    pub data: Option<JsonValue>,
}
impl InputEntry {
    pub fn new(kind: impl Into<JsonString>) -> Self {
        Self {
            kind: kind.into(),
            data: None,
        }
    }
    pub(crate) fn to_json(&self) -> JsonValue {
        let mut value = wire::object();
        wire::put(&mut value, "kind", &self.kind);
        wire::put_optional(&mut value, "data", &self.data);
        value
    }
    pub(crate) fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(Self {
            kind: wire::field(value, "kind")?,
            data: wire::optional(value, "data")?,
        })
    }
}
wire::raw_serde!(InputEntry);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WhenBusy {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
    #[serde(rename = "reject")]
    Reject,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueMode {
    #[serde(rename = "all")]
    All,
    #[default]
    #[serde(rename = "one-at-a-time")]
    OneAtATime,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueModes {
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
}
impl From<&super::settings::Settings> for QueueModes {
    fn from(settings: &super::settings::Settings) -> Self {
        let mode = |value: &JsonString| {
            if value.equals_str("all") {
                QueueMode::All
            } else {
                QueueMode::OneAtATime
            }
        };
        Self {
            steering_mode: mode(&settings.steering_mode),
            follow_up_mode: mode(&settings.follow_up_mode),
        }
    }
}
#[derive(Clone, Debug)]
pub enum SubmissionDraft {
    Input {
        request_id: Option<JsonString>,
        content: UserInput,
        when_busy: Option<WhenBusy>,
        entry: Option<InputEntry>,
    },
    Write {
        request_id: Option<JsonString>,
        entry: EntryDraft,
    },
}
impl SubmissionDraft {
    pub fn input(content: impl Into<JsonValue>) -> Self {
        Self::Input {
            request_id: None,
            content: content.into(),
            when_busy: None,
            entry: None,
        }
    }
    pub fn write(entry: EntryDraft) -> Self {
        Self::Write {
            request_id: None,
            entry,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Input { .. } => "input",
            Self::Write { .. } => "write",
        }
    }
    pub fn request_id(&self) -> Option<&JsonString> {
        match self {
            Self::Input { request_id, .. } | Self::Write { request_id, .. } => request_id.as_ref(),
        }
    }
    fn to_json(&self) -> JsonValue {
        let mut value = wire::object();
        wire::put(&mut value, "type", &self.kind());
        wire::put_optional(&mut value, "requestId", &self.request_id());
        match self {
            Self::Input {
                content,
                when_busy,
                entry,
                ..
            } => {
                wire::put(&mut value, "content", content);
                wire::put_optional(&mut value, "whenBusy", when_busy);
                wire::put_optional(&mut value, "entry", entry);
            }
            Self::Write { entry, .. } => wire::put(&mut value, "entry", entry),
        }
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        let request_id = wire::optional(value, "requestId")?;
        Ok(match wire::tag(value, "type")?.as_str() {
            "input" => Self::Input {
                request_id,
                content: wire::field(value, "content")?,
                when_busy: wire::optional(value, "whenBusy")?,
                entry: wire::optional(value, "entry")?,
            },
            "write" => Self::Write {
                request_id,
                entry: wire::field(value, "entry")?,
            },
            other => return Err(format!("Unknown submission draft {other}")),
        })
    }
}
wire::raw_serde!(SubmissionDraft);

/// Only done/unanswered records can construct this receipt.
#[derive(Clone, Debug)]
pub struct SettledSubmissionRecord(SubmissionRecord);
impl SettledSubmissionRecord {
    pub fn as_record(&self) -> &SubmissionRecord {
        &self.0
    }
    pub fn into_record(self) -> SubmissionRecord {
        self.0
    }
}
impl TryFrom<SubmissionRecord> for SettledSubmissionRecord {
    type Error = Error;
    fn try_from(record: SubmissionRecord) -> Result<Self> {
        if matches!(
            record.state.status(),
            SubmissionStatus::Done | SubmissionStatus::Unanswered
        ) {
            Ok(Self(record))
        } else {
            Err(Error::message(format!(
                "Submission {} has not settled",
                record.id()
            )))
        }
    }
}
impl std::ops::Deref for SettledSubmissionRecord {
    type Target = SubmissionRecord;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Serialize for SettledSubmissionRecord {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for SettledSubmissionRecord {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Self::try_from(SubmissionRecord::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}
