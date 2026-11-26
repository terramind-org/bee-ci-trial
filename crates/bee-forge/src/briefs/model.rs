//! The stored evidence record. Field numbers follow the proto rule: never renumbered or
//! retyped.

use bee_proto::v1::{Evidence, EvidenceAssetRole, EvidenceSpec, EvidenceState};

/// One evidence item: the item as served (without URLs and staleness, which are computed on
/// every read) plus what the API does not expose.
#[derive(Clone, PartialEq, prost::Message)]
pub struct EvidenceRec {
    /// The item as served.
    #[prost(message, optional, tag = "1")]
    pub evidence: Option<Evidence>,
    /// Org of the repo (blob namespace, events and audit).
    #[prost(string, tag = "2")]
    pub org_id: String,
    /// Per asset, in the order of `evidence.assets`.
    #[prost(message, repeated, tag = "3")]
    pub assets: Vec<AssetRec>,
    /// When a pending item is deleted (ms); 0 once ready.
    #[prost(uint64, tag = "4")]
    pub expires_ms: u64,
}

/// What the server keeps about one asset.
#[derive(Clone, PartialEq, prost::Message)]
pub struct AssetRec {
    /// `bee.v1.EvidenceAssetRole`.
    #[prost(int32, tag = "1")]
    pub role: i32,
    /// CI blob id of the bytes; empty until received.
    #[prost(string, tag = "2")]
    pub blob: String,
    /// Declared content type (the kind's canonical type).
    #[prost(string, tag = "3")]
    pub declared_type: String,
    /// Declared size.
    #[prost(uint64, tag = "4")]
    pub declared_size: u64,
    /// Lower-case hex SHA-256 of the received bytes, computed by the server.
    #[prost(string, tag = "5")]
    pub sha256: String,
    /// Received size.
    #[prost(uint64, tag = "6")]
    pub size: u64,
}

impl EvidenceRec {
    /// The item (always present in a stored record).
    pub fn evidence(&self) -> &Evidence {
        static EMPTY: std::sync::OnceLock<Evidence> = std::sync::OnceLock::new();
        self.evidence
            .as_ref()
            .unwrap_or_else(|| EMPTY.get_or_init(Evidence::default))
    }

    /// The item, mutable.
    pub fn evidence_mut(&mut self) -> &mut Evidence {
        self.evidence.get_or_insert_with(Evidence::default)
    }

    /// The item's spec.
    pub fn spec(&self) -> EvidenceSpec {
        self.evidence().spec.clone().unwrap_or_default()
    }

    /// The item's state.
    pub fn state(&self) -> EvidenceState {
        EvidenceState::try_from(self.evidence().state).unwrap_or(EvidenceState::Unspecified)
    }

    /// Bytes the item's assets take (declared while pending, received once ready).
    pub fn bytes(&self) -> u64 {
        self.assets
            .iter()
            .map(|a| a.size.max(a.declared_size))
            .sum()
    }
}

impl AssetRec {
    /// The asset's role.
    pub fn role(&self) -> EvidenceAssetRole {
        EvidenceAssetRole::try_from(self.role).unwrap_or(EvidenceAssetRole::Unspecified)
    }
}
