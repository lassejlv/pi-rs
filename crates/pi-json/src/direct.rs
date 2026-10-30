//! Direct serde bridges between typed values and the strict `Value`, with no JSON text in between.
//!
//! Strings stay lossless: `Value`'s own `Serialize`/`Deserialize` impls recognise this module's deserializer and
//! serializer through a private newtype marker (much as `serde_json::value::RawValue` does) and hand over the
//! `Value` itself, including lone UTF-16 surrogates. Every other deserializer or serializer gets the text-based
//! behaviour those impls always had.
use crate::strict::{JsonString, Map, Value, object_entries};
use serde::{
    Serialize,
    de::{self, DeserializeOwned, Deserializer, IntoDeserializer, Visitor},
    ser::{self, Serializer},
};
use std::{cell::RefCell, fmt, sync::Arc};

/// Name of the private newtype that carries a whole `Value` through a serde (de)serializer.
pub const MARKER: &str = "$pi_json::private::Value";

thread_local! {
    /// Set by `Value::serialize` immediately before it offers itself to the serializer.
    static SERIALIZE_OFFER: RefCell<Option<Value>> = const { RefCell::new(None) };
    /// Set by this module's deserializer immediately before it calls a visitor for the marker.
    static DESERIALIZE_OFFER: RefCell<Option<Value>> = const { RefCell::new(None) };
}
pub(crate) fn offer_for_serialize(value: &Value) {
    SERIALIZE_OFFER.with(|slot| *slot.borrow_mut() = Some(value.clone()));
}
pub(crate) fn withdraw_serialize_offer() -> Option<Value> {
    SERIALIZE_OFFER.with(|slot| slot.borrow_mut().take())
}
pub(crate) fn take_deserialize_offer() -> Option<Value> {
    DESERIALIZE_OFFER.with(|slot| slot.borrow_mut().take())
}
fn offer_for_deserialize(value: &Value) {
    DESERIALIZE_OFFER.with(|slot| *slot.borrow_mut() = Some(value.clone()));
}

#[derive(Debug, Clone)]
pub struct Error(String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl de::Error for Error {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }
}
impl ser::Error for Error {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }
}

/// Decode `T` from `value` without going through JSON text.
pub fn from_value_direct<T: DeserializeOwned>(value: &Value) -> Result<T, String> {
    T::deserialize(ValueDeserializer(value)).map_err(|error| error.0)
}
/// Encode `value` as a `Value` without going through JSON text.
pub fn to_value_direct<T: Serialize + ?Sized>(value: &T) -> Result<Value, String> {
    let result = value.serialize(ValueSerializer).map_err(|error| error.0);
    withdraw_serialize_offer();
    result
}

// ─── Deserializer ───

pub struct ValueDeserializer<'a>(&'a Value);

fn visit_number<'de, V: Visitor<'de>>(
    number: &serde_json::Number,
    visitor: V,
) -> Result<V::Value, Error> {
    if let Some(n) = number.as_u64() {
        return visitor.visit_u64(n);
    }
    if let Some(n) = number.as_i64() {
        return visitor.visit_i64(n);
    }
    let float = number.as_f64().unwrap_or(f64::NAN);
    // Integral doubles read as integers, as they do in their JSON text.
    if float.fract() == 0.0 && float.abs() < 9_007_199_254_740_992.0 {
        return if float >= 0.0 {
            visitor.visit_u64(float as u64)
        } else {
            visitor.visit_i64(float as i64)
        };
    }
    visitor.visit_f64(float)
}
fn checked(string: &JsonString) -> Result<String, Error> {
    string.to_string_checked().map_err(|_| {
        <Error as de::Error>::custom("lone surrogate in a string that was read as UTF-8")
    })
}

impl<'de> Deserializer<'de> for ValueDeserializer<'_> {
    type Error = Error;
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Bool(b) => visitor.visit_bool(*b),
            Value::Number(n) => visit_number(n, visitor),
            Value::String(s) => visitor.visit_string(checked(s)?),
            Value::Array(items) => {
                let mut access = SeqAccess { iter: items.iter() };
                let value = visitor.visit_seq(&mut access)?;
                if access.iter.len() == 0 {
                    Ok(value)
                } else {
                    Err(de::Error::invalid_length(
                        items.len(),
                        &"fewer elements in array",
                    ))
                }
            }
            Value::Object(map) => {
                let mut access = MapAccess {
                    entries: object_entries(map).collect::<Vec<_>>().into_iter(),
                    pending: None,
                };
                let value = visitor.visit_map(&mut access)?;
                if access.entries.len() == 0 {
                    Ok(value)
                } else {
                    Err(de::Error::invalid_length(
                        map.len(),
                        &"fewer elements in map",
                    ))
                }
            }
        }
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        if name == MARKER {
            offer_for_deserialize(self.0);
        }
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self.0 {
            Value::String(s) => {
                let variant: de::value::StringDeserializer<Error> = checked(s)?.into_deserializer();
                visitor.visit_enum(variant)
            }
            Value::Object(map) if map.len() == 1 => {
                let (key, value) = map.iter().next().expect("one entry");
                visitor.visit_enum(EnumAccess { key, value })
            }
            _ => Err(de::Error::custom(
                "expected a string or a single-key object for an enum",
            )),
        }
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit unit_struct seq
        tuple tuple_struct map struct identifier
    }
}

struct SeqAccess<'a> {
    iter: std::slice::Iter<'a, Value>,
}
impl<'de> de::SeqAccess<'de> for &mut SeqAccess<'_> {
    type Error = Error;
    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        match self.iter.next() {
            Some(value) => seed.deserialize(ValueDeserializer(value)).map(Some),
            None => Ok(None),
        }
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

struct MapAccess<'a> {
    entries: std::vec::IntoIter<(&'a JsonString, &'a Value)>,
    pending: Option<&'a Value>,
}
impl<'de> de::MapAccess<'de> for &mut MapAccess<'_> {
    type Error = Error;
    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        match self.entries.next() {
            Some((key, value)) => {
                self.pending = Some(value);
                seed.deserialize(KeyDeserializer(key)).map(Some)
            }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let value = self
            .pending
            .take()
            .ok_or_else(|| <Error as de::Error>::custom("value without a key"))?;
        seed.deserialize(ValueDeserializer(value))
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

/// An object key: a string, readable as an integer when the target asks for one (as serde_json does).
struct KeyDeserializer<'a>(&'a JsonString);
macro_rules! key_integer {
    ($($method:ident => $visit:ident as $ty:ty),*) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            match checked(self.0)?.parse::<$ty>() {
                Ok(n) => visitor.$visit(n),
                Err(_) => self.deserialize_any(visitor),
            }
        }
    )*};
}
impl<'de> Deserializer<'de> for KeyDeserializer<'_> {
    type Error = Error;
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_string(checked(self.0)?)
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        if name == MARKER {
            offer_for_deserialize(&Value::String(self.0.clone()));
        }
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_some(self)
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let variant: de::value::StringDeserializer<Error> = checked(self.0)?.into_deserializer();
        visitor.visit_enum(variant)
    }
    key_integer!(
        deserialize_i8 => visit_i64 as i64, deserialize_i16 => visit_i64 as i64, deserialize_i32 => visit_i64 as i64,
        deserialize_i64 => visit_i64 as i64, deserialize_u8 => visit_u64 as u64, deserialize_u16 => visit_u64 as u64,
        deserialize_u32 => visit_u64 as u64, deserialize_u64 => visit_u64 as u64
    );
    serde::forward_to_deserialize_any! {
        bool i128 u128 f32 f64 char str string bytes byte_buf unit unit_struct seq tuple tuple_struct map struct
        identifier ignored_any
    }
}

struct EnumAccess<'a> {
    key: &'a JsonString,
    value: &'a Value,
}
impl<'de, 'a> de::EnumAccess<'de> for EnumAccess<'a> {
    type Error = Error;
    type Variant = VariantAccess<'a>;
    fn variant_seed<V: de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, VariantAccess<'a>), Error> {
        let variant = seed.deserialize(KeyDeserializer(self.key))?;
        Ok((variant, VariantAccess { value: self.value }))
    }
}
struct VariantAccess<'a> {
    value: &'a Value,
}
impl<'de> de::VariantAccess<'de> for VariantAccess<'_> {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        match self.value {
            Value::Null => Ok(()),
            _ => Err(de::Error::custom("expected a unit variant")),
        }
    }
    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(ValueDeserializer(self.value))
    }
    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        ValueDeserializer(self.value).deserialize_any(visitor)
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        ValueDeserializer(self.value).deserialize_any(visitor)
    }
}

// ─── Serializer ───

pub struct ValueSerializer;

fn number<N: Into<serde_json::Number>>(n: N) -> Value {
    Value::Number(n.into())
}

impl Serializer for ValueSerializer {
    type Ok = Value;
    type Error = Error;
    type SerializeSeq = SeqBuilder;
    type SerializeTuple = SeqBuilder;
    type SerializeTupleStruct = SeqBuilder;
    type SerializeTupleVariant = VariantSeqBuilder;
    type SerializeMap = MapBuilder;
    type SerializeStruct = MapBuilder;
    type SerializeStructVariant = VariantMapBuilder;

    fn serialize_bool(self, v: bool) -> Result<Value, Error> {
        Ok(Value::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> Result<Value, Error> {
        Ok(number(v))
    }
    fn serialize_i16(self, v: i16) -> Result<Value, Error> {
        Ok(number(v))
    }
    fn serialize_i32(self, v: i32) -> Result<Value, Error> {
        Ok(number(v))
    }
    fn serialize_i64(self, v: i64) -> Result<Value, Error> {
        Ok(number(v))
    }
    fn serialize_u8(self, v: u8) -> Result<Value, Error> {
        Ok(number(v))
    }
    fn serialize_u16(self, v: u16) -> Result<Value, Error> {
        Ok(number(v))
    }
    fn serialize_u32(self, v: u32) -> Result<Value, Error> {
        Ok(number(v))
    }
    fn serialize_u64(self, v: u64) -> Result<Value, Error> {
        Ok(number(v))
    }
    fn serialize_f32(self, v: f32) -> Result<Value, Error> {
        self.serialize_f64(f64::from(v))
    }
    fn serialize_f64(self, v: f64) -> Result<Value, Error> {
        // Like serde_json, a non-finite number becomes null.
        Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
    }
    fn serialize_char(self, v: char) -> Result<Value, Error> {
        Ok(Value::String(JsonString::from(v.to_string())))
    }
    fn serialize_str(self, v: &str) -> Result<Value, Error> {
        Ok(Value::String(JsonString::from(v)))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Value, Error> {
        Ok(Value::Array(Arc::new(
            v.iter().map(|byte| number(*byte)).collect(),
        )))
    }
    fn serialize_none(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Value, Error> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Value, Error> {
        self.serialize_str(variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Value, Error> {
        if name == MARKER
            && let Some(offered) = withdraw_serialize_offer()
        {
            return Ok(offered);
        }
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value, Error> {
        let mut map = Map::new();
        map.insert(JsonString::from(variant), value.serialize(ValueSerializer)?);
        Ok(Value::Object(Arc::new(map)))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<SeqBuilder, Error> {
        Ok(SeqBuilder(Vec::with_capacity(len.unwrap_or(0))))
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqBuilder, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<SeqBuilder, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<VariantSeqBuilder, Error> {
        Ok(VariantSeqBuilder {
            variant,
            items: Vec::with_capacity(len),
        })
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<MapBuilder, Error> {
        Ok(MapBuilder {
            map: Map::new(),
            key: None,
        })
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<MapBuilder, Error> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<VariantMapBuilder, Error> {
        Ok(VariantMapBuilder {
            variant,
            map: Map::new(),
        })
    }
}

pub struct SeqBuilder(Vec<Value>);
impl ser::SerializeSeq for SeqBuilder {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.0.push(value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Array(Arc::new(self.0)))
    }
}
impl ser::SerializeTuple for SeqBuilder {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        ser::SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Value, Error> {
        ser::SerializeSeq::end(self)
    }
}
impl ser::SerializeTupleStruct for SeqBuilder {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        ser::SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Value, Error> {
        ser::SerializeSeq::end(self)
    }
}
pub struct VariantSeqBuilder {
    variant: &'static str,
    items: Vec<Value>,
}
impl ser::SerializeTupleVariant for VariantSeqBuilder {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.items.push(value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        let mut map = Map::new();
        map.insert(
            JsonString::from(self.variant),
            Value::Array(Arc::new(self.items)),
        );
        Ok(Value::Object(Arc::new(map)))
    }
}
pub struct MapBuilder {
    map: Map,
    key: Option<JsonString>,
}
impl ser::SerializeMap for MapBuilder {
    type Ok = Value;
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.key = Some(key.serialize(KeySerializer)?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        let key = self
            .key
            .take()
            .ok_or_else(|| <Error as ser::Error>::custom("value without a key"))?;
        self.map.insert(key, value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Object(Arc::new(self.map)))
    }
}
impl ser::SerializeStruct for MapBuilder {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.map
            .insert(JsonString::from(key), value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Object(Arc::new(self.map)))
    }
}
pub struct VariantMapBuilder {
    variant: &'static str,
    map: Map,
}
impl ser::SerializeStructVariant for VariantMapBuilder {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.map
            .insert(JsonString::from(key), value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        let mut outer = Map::new();
        outer.insert(
            JsonString::from(self.variant),
            Value::Object(Arc::new(self.map)),
        );
        Ok(Value::Object(Arc::new(outer)))
    }
}

/// Serializes a map key: strings, plus integers and unit variants written as strings, like serde_json.
struct KeySerializer;
fn key_error() -> Error {
    <Error as ser::Error>::custom("key must be a string")
}
macro_rules! key_integer {
    ($($method:ident($ty:ty)),*) => {$(
        fn $method(self, v: $ty) -> Result<JsonString, Error> {
            Ok(JsonString::from(v.to_string()))
        }
    )*};
}
macro_rules! key_unsupported {
    ($($method:ident($($ty:ty)?)),*) => {$(
        fn $method(self $(, _: $ty)?) -> Result<JsonString, Error> {
            Err(key_error())
        }
    )*};
}
impl Serializer for KeySerializer {
    type Ok = JsonString;
    type Error = Error;
    type SerializeSeq = ser::Impossible<JsonString, Error>;
    type SerializeTuple = ser::Impossible<JsonString, Error>;
    type SerializeTupleStruct = ser::Impossible<JsonString, Error>;
    type SerializeTupleVariant = ser::Impossible<JsonString, Error>;
    type SerializeMap = ser::Impossible<JsonString, Error>;
    type SerializeStruct = ser::Impossible<JsonString, Error>;
    type SerializeStructVariant = ser::Impossible<JsonString, Error>;
    key_integer!(
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64)
    );
    key_unsupported!(
        serialize_bool(bool),
        serialize_f32(f32),
        serialize_f64(f64),
        serialize_bytes(&[u8]),
        serialize_none(),
        serialize_unit(),
        serialize_unit_struct(&'static str)
    );
    fn serialize_char(self, v: char) -> Result<JsonString, Error> {
        Ok(JsonString::from(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<JsonString, Error> {
        Ok(JsonString::from(v))
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<JsonString, Error> {
        value.serialize(self)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<JsonString, Error> {
        Ok(JsonString::from(variant))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<JsonString, Error> {
        if name == MARKER
            && let Some(offered) = withdraw_serialize_offer()
        {
            return match offered {
                Value::String(key) => Ok(key),
                _ => Err(key_error()),
            };
        }
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<JsonString, Error> {
        Err(key_error())
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Err(key_error())
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Error> {
        Err(key_error())
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        Err(key_error())
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        Err(key_error())
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Err(key_error())
    }
    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        Err(key_error())
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Err(key_error())
    }
}
