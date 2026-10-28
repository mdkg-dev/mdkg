//! Reject duplicate object keys at every depth instead of silently picking one.

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

struct Unique(Value);

impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Json;
        impl<'de> Visitor<'de> for Json {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_bool<E: de::Error>(self, b: bool) -> Result<Unique, E> {
                Ok(Unique(Value::Bool(b)))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Unique, E> {
                Ok(Unique(Value::Number(n.into())))
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<Unique, E> {
                Ok(Unique(Value::Number(n.into())))
            }
            fn visit_f64<E: de::Error>(self, n: f64) -> Result<Unique, E> {
                Number::from_f64(n)
                    .map(|n| Unique(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Unique, E> {
                Ok(Unique(Value::String(s.into())))
            }
            fn visit_string<E: de::Error>(self, s: String) -> Result<Unique, E> {
                Ok(Unique(Value::String(s)))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Unique, A::Error> {
                let mut out = Vec::new();
                while let Some(Unique(v)) = seq.next_element()? {
                    out.push(v);
                }
                Ok(Unique(Value::Array(out)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Unique, A::Error> {
                let mut out = Map::new();
                while let Some((k, Unique(v))) = map.next_entry::<String, Unique>()? {
                    if out.contains_key(&k) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    out.insert(k, v);
                }
                Ok(Unique(Value::Object(out)))
            }
        }
        d.deserialize_any(Json)
    }
}

pub(super) fn parse(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    serde_json::from_slice::<Unique>(bytes).map(|v| v.0)
}
