//! Canonical JSON: object keys sorted by byte order, no insignificant
//! whitespace. It does not depend on serde_json's map ordering, which feature
//! unification elsewhere in the workspace could switch to insertion order.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Serialize `v` canonically.
pub fn to_canonical_string(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, &mut out);
    out
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_scalar(&Value::String((*k).clone()), out);
                out.push(':');
                write_value(&map[k.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        scalar => write_scalar(scalar, out),
    }
}

fn write_scalar(v: &Value, out: &mut String) {
    // A scalar's serialization cannot fail.
    out.push_str(&serde_json::to_string(v).expect("serialize a JSON scalar"));
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `sha256:<hex>` of the canonical form of `v`.
pub fn content_hash(v: &Value) -> String {
    format!("sha256:{}", sha256_hex(to_canonical_string(v).as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keys_sort_at_every_depth_whatever_the_parse_order() {
        let a: Value =
            serde_json::from_str(r#"{"b":1,"a":{"z":[{"y":2,"x":3}],"c":"d"}}"#).unwrap();
        let b: Value =
            serde_json::from_str(r#"{"a":{"c":"d","z":[{"x":3,"y":2}]},"b":1}"#).unwrap();
        let expected = r#"{"a":{"c":"d","z":[{"x":3,"y":2}]},"b":1}"#;
        assert_eq!(to_canonical_string(&a), expected);
        assert_eq!(to_canonical_string(&b), expected);
    }

    #[test]
    fn array_order_is_significant() {
        let a: Value = serde_json::from_str("[1,2]").unwrap();
        let b: Value = serde_json::from_str("[2,1]").unwrap();
        assert_ne!(content_hash(&a), content_hash(&b));
    }

    #[test]
    fn strings_are_json_escaped() {
        let v = serde_json::json!({"q\"k": "line\nbreak"});
        assert_eq!(to_canonical_string(&v), r#"{"q\"k":"line\nbreak"}"#);
    }

    #[test]
    fn sha256_matches_the_published_abc_vector() {
        // FIPS 180-2 appendix B.1.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
