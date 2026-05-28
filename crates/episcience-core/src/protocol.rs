use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A versioned lab protocol (SOP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Protocol {
    pub id: Uuid,
    pub title: String,
    pub version: i32,
    pub authored_by: Uuid,
    pub steps: Vec<ProtocolStep>,
    pub equipment: Vec<String>,
    pub safety_notes: Option<String>,
    pub supersedes: Option<Uuid>,
    pub labels: Vec<String>,
    pub properties: serde_json::Value,
    pub content_hash: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Structured section vocabulary for foundation-agent splicing. See
    /// [`ProtocolSections`]. Empty on legacy / unset rows.
    #[serde(default)]
    pub sections: ProtocolSections,
}

/// A single step in a protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolStep {
    pub order: i32,
    pub instruction: String,
    #[serde(default)]
    pub duration_minutes: Option<f64>,
    #[serde(default)]
    pub temperature_c: Option<f64>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Structured section vocabulary for a Protocol. Mirrors SciLink's
/// foundation-agent section set so downstream agents can splice
/// guidance at known decision points. All five named sections are
/// optional. Off-vocabulary keys submitted on `POST /protocols` are
/// preserved under `extras` and surfaced via a warning header.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ProtocolSections {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpretation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<String>,
    /// Off-vocabulary keys submitted by clients. The API layer warns
    /// when populated but does not reject. Empty when no off-vocab keys
    /// were sent.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub extras: std::collections::HashMap<String, String>,
}

impl ProtocolSections {
    /// Build a `ProtocolSections` from an arbitrary JSON value (typically
    /// the raw `sections` field from a `POST /protocols` request body).
    /// Off-vocabulary keys move to `extras`; non-string values for known
    /// keys are dropped (no failure). Returns (parsed sections, list of
    /// off-vocab keys observed for warning purposes).
    pub fn from_value(v: &serde_json::Value) -> (Self, Vec<String>) {
        let mut s = Self::default();
        let mut off_vocab = Vec::new();
        if let serde_json::Value::Object(map) = v {
            for (k, val) in map {
                let text = val.as_str().map(String::from);
                match k.as_str() {
                    "overview" => s.overview = text,
                    "planning" => s.planning = text,
                    "implementation" => s.implementation = text,
                    "interpretation" => s.interpretation = text,
                    "validation" => s.validation = text,
                    _ => {
                        if let Some(t) = text {
                            s.extras.insert(k.clone(), t);
                            off_vocab.push(k.clone());
                        }
                    }
                }
            }
        }
        (s, off_vocab)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_value_parses_all_five_named_sections() {
        let v = json!({
            "overview": "ov",
            "planning": "pl",
            "implementation": "im",
            "interpretation": "in",
            "validation": "va",
        });
        let (s, off) = ProtocolSections::from_value(&v);
        assert!(off.is_empty(), "no off-vocab keys expected");
        assert_eq!(s.overview.as_deref(), Some("ov"));
        assert_eq!(s.planning.as_deref(), Some("pl"));
        assert_eq!(s.implementation.as_deref(), Some("im"));
        assert_eq!(s.interpretation.as_deref(), Some("in"));
        assert_eq!(s.validation.as_deref(), Some("va"));
        assert!(s.extras.is_empty());
    }

    #[test]
    fn from_value_routes_off_vocab_to_extras_and_reports_keys() {
        let v = json!({
            "overview": "ok",
            "weird": "leftover",
            "another_extra": "value",
        });
        let (s, off) = ProtocolSections::from_value(&v);
        assert_eq!(s.overview.as_deref(), Some("ok"));
        assert_eq!(s.extras.get("weird").map(String::as_str), Some("leftover"));
        assert_eq!(
            s.extras.get("another_extra").map(String::as_str),
            Some("value")
        );
        // Off-vocab key reporting (order-independent).
        let mut sorted = off;
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["another_extra".to_string(), "weird".to_string()]
        );
    }

    #[test]
    fn from_value_drops_non_string_known_keys_without_failing() {
        let v = json!({
            "overview": 42,
            "planning": "ok",
        });
        let (s, off) = ProtocolSections::from_value(&v);
        assert!(off.is_empty(), "non-string known key should not be flagged");
        assert!(s.overview.is_none(), "non-string overview dropped");
        assert_eq!(s.planning.as_deref(), Some("ok"));
    }

    #[test]
    fn default_roundtrips_through_json_as_empty_object() {
        let s = ProtocolSections::default();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v, json!({}));
        let back: ProtocolSections = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }
}
