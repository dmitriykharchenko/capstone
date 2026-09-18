use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use std::fmt;

use crate::{protocol::ErrorCode, registry::BackendResult};

// Unlike Value's normal deserializer, reject duplicate keys at EVERY depth.
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Strict;
        impl<'de> Visitor<'de> for Strict {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("strict JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Unique, E> {
                Number::from_f64(v)
                    .map(|n| Unique(Value::Number(n)))
                    .ok_or_else(|| E::custom("nonfinite"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Unique, A::Error> {
                let mut values = Vec::new();
                while let Some(Unique(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(Unique(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Unique, A::Error> {
                let mut values = Map::new();
                while let Some((key, Unique(value))) = map.next_entry::<String, Unique>()? {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate key"));
                    }
                }
                Ok(Unique(Value::Object(values)))
            }
        }
        d.deserialize_any(Strict)
    }
}

pub(super) fn value(text: &str, max: usize) -> BackendResult<Value> {
    if text.len() > max {
        return Err(ErrorCode::ResponseTooLarge);
    }
    serde_json::from_str::<Unique>(text)
        .map(|v| v.0)
        .map_err(|_| ErrorCode::InvalidParams)
}

// Exact round-trip also rejects positional struct arrays, missing nullable fields,
// unknown fields swallowed by foundation DTOs, and coercions such as score=1.0.
pub(super) fn exact<T: for<'de> Deserialize<'de> + Serialize>(value: Value) -> BackendResult<T> {
    let parsed: T = serde_json::from_value(value.clone()).map_err(|_| ErrorCode::InvalidParams)?;
    if serde_json::to_value(&parsed).map_err(|_| ErrorCode::Internal)? != value {
        return Err(ErrorCode::InvalidParams);
    }
    Ok(parsed)
}

pub(super) fn nonempty(text: &str) -> bool {
    !text
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .is_empty()
}
