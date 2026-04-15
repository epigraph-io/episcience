use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Physical sample tracked in the lab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub id: Uuid,
    pub name: String,
    pub sample_type: SampleType,
    pub status: SampleStatus,
    pub parent_sample_id: Option<Uuid>,
    pub prepared_by: Uuid,
    pub preparation_date: DateTime<Utc>,
    pub expiry_date: Option<DateTime<Utc>>,
    pub storage_location: Option<String>,
    pub quantity: Option<Quantity>,
    pub hazard_info: serde_json::Value,
    pub labels: Vec<String>,
    pub properties: serde_json::Value,
    pub content_hash: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleType {
    Biological,
    Chemical,
    Material,
    Composite,
}

impl SampleType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Biological => "biological",
            Self::Chemical => "chemical",
            Self::Material => "material",
            Self::Composite => "composite",
        }
    }
}

impl std::fmt::Display for SampleType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SampleType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "biological" => Ok(Self::Biological),
            "chemical" => Ok(Self::Chemical),
            "material" => Ok(Self::Material),
            "composite" => Ok(Self::Composite),
            other => Err(format!("Unknown sample type: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleStatus {
    Prepared,
    InUse,
    Consumed,
    Disposed,
    Archived,
}

impl SampleStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::InUse => "in_use",
            Self::Consumed => "consumed",
            Self::Disposed => "disposed",
            Self::Archived => "archived",
        }
    }

    /// Validate that a status transition is allowed.
    pub fn can_transition_to(&self, next: Self) -> bool {
        use SampleStatus::*;
        matches!(
            (self, next),
            (Prepared, InUse)
                | (Prepared, Disposed)
                | (Prepared, Archived)
                | (InUse, Consumed)
                | (InUse, Disposed)
                | (InUse, Archived)
                | (Consumed, Archived)
                | (Disposed, Archived)
        )
    }
}

impl std::fmt::Display for SampleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SampleStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "prepared" => Ok(Self::Prepared),
            "in_use" => Ok(Self::InUse),
            "consumed" => Ok(Self::Consumed),
            "disposed" => Ok(Self::Disposed),
            "archived" => Ok(Self::Archived),
            other => Err(format!("Unknown sample status: {other}")),
        }
    }
}

/// Quantity with value and unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quantity {
    pub value: f64,
    pub unit: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_transitions() {
        use SampleStatus::*;
        assert!(Prepared.can_transition_to(InUse));
        assert!(InUse.can_transition_to(Consumed));
        assert!(InUse.can_transition_to(Disposed));
        assert!(!Consumed.can_transition_to(InUse));
        assert!(!Disposed.can_transition_to(Prepared));
        assert!(Consumed.can_transition_to(Archived));
    }

    #[test]
    fn test_sample_type_roundtrip() {
        let t = SampleType::Chemical;
        let s = t.as_str();
        let parsed: SampleType = s.parse().unwrap();
        assert_eq!(t, parsed);
    }
}
