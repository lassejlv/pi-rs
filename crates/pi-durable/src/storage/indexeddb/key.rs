//! The IndexedDB key model used by the durable schema: valid keys, their
//! cross-type ordering, ranges and key-path extraction from stored rows.
use crate::{JsonString, JsonValue, wire};
use std::cmp::Ordering;

/// A valid IndexedDB key. The durable schema only stores numbers, strings and
/// arrays of those, so dates and binary keys are not modelled.
#[derive(Clone, Debug)]
pub enum IdbKey {
    Number(f64),
    String(JsonString),
    Array(Vec<IdbKey>),
}
impl IdbKey {
    fn rank(&self) -> u8 {
        match self {
            Self::Number(_) => 0,
            Self::String(_) => 1,
            Self::Array(_) => 2,
        }
    }
    /// The key a JSON value denotes, or `None` when IndexedDB rejects it as a key.
    pub fn from_json(value: &JsonValue) -> Option<Self> {
        match value {
            JsonValue::Number(n) => n.as_f64().filter(|n| !n.is_nan()).map(Self::Number),
            JsonValue::String(s) => Some(Self::String(s.clone())),
            JsonValue::Array(items) => items
                .iter()
                .map(Self::from_json)
                .collect::<Option<Vec<_>>>()
                .map(Self::Array),
            _ => None,
        }
    }
    pub fn to_json(&self) -> JsonValue {
        match self {
            Self::Number(n) => number(*n),
            Self::String(s) => JsonValue::String(s.clone()),
            Self::Array(items) => JsonValue::Array(std::sync::Arc::new(
                items.iter().map(Self::to_json).collect(),
            )),
        }
    }
}
/// JavaScript numbers that are safe integers keep an integral JSON spelling.
pub fn number(n: f64) -> JsonValue {
    if n.fract() == 0.0 && n.abs() <= crate::MAX_SAFE_INTEGER as f64 {
        JsonValue::Number((n as i64).into())
    } else {
        serde_json::Number::from_f64(n).map_or(JsonValue::Null, JsonValue::Number)
    }
}
impl PartialEq for IdbKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for IdbKey {}
impl PartialOrd for IdbKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for IdbKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => a.total_cmp(b),
            // Strings compare by UTF-16 code unit, like JavaScript.
            (Self::String(a), Self::String(b)) => a.units().cmp(b.units()),
            (Self::Array(a), Self::Array(b)) => a.iter().cmp(b.iter()),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}
impl From<i64> for IdbKey {
    fn from(value: i64) -> Self {
        Self::Number(value as f64)
    }
}
impl From<&str> for IdbKey {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}
impl From<&JsonString> for IdbKey {
    fn from(value: &JsonString) -> Self {
        Self::String(value.clone())
    }
}
impl From<Vec<IdbKey>> for IdbKey {
    fn from(value: Vec<IdbKey>) -> Self {
        Self::Array(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyPath {
    Single(&'static str),
    Compound(&'static [&'static str]),
}
impl KeyPath {
    /// Evaluate the key path against a row. Rows lacking a component, or with a
    /// component that is not a valid key, have no key (and no index entry).
    pub fn extract(&self, row: &JsonValue) -> Option<IdbKey> {
        match self {
            Self::Single(name) => wire::get(row, name).and_then(IdbKey::from_json),
            Self::Compound(names) => names
                .iter()
                .map(|name| wire::get(row, name).and_then(IdbKey::from_json))
                .collect::<Option<Vec<_>>>()
                .map(IdbKey::Array),
        }
    }
}

/// `IDBKeyRange`; each bound carries its `open` flag.
#[derive(Clone, Debug, PartialEq)]
pub struct IdbKeyRange {
    pub lower: Option<(IdbKey, bool)>,
    pub upper: Option<(IdbKey, bool)>,
}
impl IdbKeyRange {
    pub fn only(key: IdbKey) -> Self {
        Self {
            lower: Some((key.clone(), false)),
            upper: Some((key, false)),
        }
    }
    pub fn bound(lower: IdbKey, upper: IdbKey, lower_open: bool, upper_open: bool) -> Self {
        Self {
            lower: Some((lower, lower_open)),
            upper: Some((upper, upper_open)),
        }
    }
    pub fn lower_bound(key: IdbKey, open: bool) -> Self {
        Self {
            lower: Some((key, open)),
            upper: None,
        }
    }
    pub fn upper_bound(key: IdbKey, open: bool) -> Self {
        Self {
            lower: None,
            upper: Some((key, open)),
        }
    }
    pub fn contains(&self, key: &IdbKey) -> bool {
        let above = self
            .lower
            .as_ref()
            .is_none_or(|(lower, open)| match key.cmp(lower) {
                Ordering::Greater => true,
                Ordering::Equal => !open,
                Ordering::Less => false,
            });
        let below = self
            .upper
            .as_ref()
            .is_none_or(|(upper, open)| match key.cmp(upper) {
                Ordering::Less => true,
                Ordering::Equal => !open,
                Ordering::Greater => false,
            });
        above && below
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Next,
    Prev,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_numbers_before_strings_before_arrays() {
        let mut keys = vec![
            IdbKey::from(vec![IdbKey::from(1)]),
            IdbKey::from("a"),
            IdbKey::from(10),
            IdbKey::from(vec![]),
            IdbKey::from(2),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                IdbKey::from(2),
                IdbKey::from(10),
                IdbKey::from("a"),
                IdbKey::from(vec![]),
                IdbKey::from(vec![IdbKey::from(1)]),
            ]
        );
    }

    #[test]
    fn compares_strings_by_utf16_code_unit() {
        // U+FFFF sorts after a surrogate pair in UTF-16, unlike in UTF-8.
        assert!(IdbKey::from("\u{1F600}") < IdbKey::from("\u{FFFF}"));
        let lone = IdbKey::String(JsonString::from_utf16(vec![0xD800]));
        assert!(lone < IdbKey::from("\u{1F600}"));
    }

    #[test]
    fn compound_key_paths_skip_rows_with_missing_or_invalid_components() {
        let path = KeyPath::Compound(&["a", "b"]);
        let row = pi_chord::json::parse(r#"{"a":1,"b":"x"}"#).unwrap();
        assert_eq!(
            path.extract(&row),
            Some(IdbKey::from(vec![IdbKey::from(1), IdbKey::from("x")]))
        );
        let missing = pi_chord::json::parse(r#"{"a":1}"#).unwrap();
        assert_eq!(path.extract(&missing), None);
        let boolean = pi_chord::json::parse(r#"{"a":1,"b":true}"#).unwrap();
        assert_eq!(path.extract(&boolean), None);
    }

    #[test]
    fn ranges_respect_open_bounds() {
        let range = IdbKeyRange::bound(IdbKey::from(1), IdbKey::from(3), true, false);
        assert!(!range.contains(&IdbKey::from(1)));
        assert!(range.contains(&IdbKey::from(2)));
        assert!(range.contains(&IdbKey::from(3)));
        assert!(IdbKeyRange::only(IdbKey::from("a")).contains(&IdbKey::from("a")));
    }
}
