//! ChangeId (SPEC §3.3): 16 bytes that name a piece of work across amend and rebase.

use std::fmt;
use std::str::FromStr;

use crate::error::{BeeError, Result, Subcode};
use crate::hex;

/// Length of a ChangeId in bytes.
pub const CHANGE_ID_LEN: usize = 16;

/// A ChangeId. Text form `k_<32 lowercase hex>`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChangeId([u8; CHANGE_ID_LEN]);

impl ChangeId {
    /// The all-zero ChangeId, used only by the empty root changeset (SPEC §4.4).
    pub const ZERO: ChangeId = ChangeId([0; CHANGE_ID_LEN]);

    /// Wraps 16 bytes. Clients choose them at random when a change is first created.
    pub fn new(bytes: [u8; CHANGE_ID_LEN]) -> ChangeId {
        ChangeId(bytes)
    }

    /// Parses the raw form, which must be exactly 16 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<ChangeId> {
        let raw: [u8; CHANGE_ID_LEN] = bytes.try_into().map_err(|_| {
            BeeError::invalid(
                Subcode::InvalidId,
                format!(
                    "change id must be {CHANGE_ID_LEN} bytes, got {}",
                    bytes.len()
                ),
            )
        })?;
        Ok(ChangeId(raw))
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8; CHANGE_ID_LEN] {
        &self.0
    }

    /// The text form.
    pub fn to_text(&self) -> String {
        format!("k_{}", hex::encode(&self.0))
    }

    /// Parses `k_<32 lowercase hex>`.
    pub fn parse(text: &str) -> Result<ChangeId> {
        let bad = || BeeError::invalid(Subcode::InvalidId, format!("not a change id: {text:?}"));
        let digits = text.strip_prefix("k_").ok_or_else(bad)?;
        if digits.len() != CHANGE_ID_LEN * 2 {
            return Err(bad());
        }
        let bytes = hex::decode(digits).ok_or_else(bad)?;
        ChangeId::from_bytes(&bytes)
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({self})")
    }
}

impl FromStr for ChangeId {
    type Err = BeeError;
    fn from_str(s: &str) -> Result<Self> {
        ChangeId::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn change_id_text_form() {
        let id = ChangeId::new([0xa5; 16]);
        assert_eq!(id.to_text(), format!("k_{}", "a5".repeat(16)));
        assert_eq!(ChangeId::parse(&id.to_text()).unwrap(), id);
        assert_eq!(id.to_string().parse::<ChangeId>().unwrap(), id);
        assert_eq!(format!("{id:?}"), format!("ChangeId({id})"));
        assert_eq!(ChangeId::ZERO.as_bytes(), &[0; 16]);
    }

    #[test]
    fn change_id_rejects_malformed() {
        for bad in [
            "".to_string(),
            "k_".into(),
            format!("k_{}", "0".repeat(31)),
            format!("k_{}", "0".repeat(33)),
            format!("K_{}", "0".repeat(32)),
            format!("k_{}", "G".repeat(32)),
            format!("c_{}", "0".repeat(32)),
        ] {
            assert!(
                ChangeId::parse(&bad)
                    .unwrap_err()
                    .is_invalid(Subcode::InvalidId),
                "{bad}"
            );
        }
        assert!(ChangeId::from_bytes(&[0; 15]).is_err());
        assert!(ChangeId::from_bytes(&[0; 16]).is_ok());
    }

    proptest! {
        #[test]
        fn change_id_round_trip(bytes in proptest::array::uniform16(any::<u8>())) {
            let id = ChangeId::new(bytes);
            prop_assert_eq!(ChangeId::parse(&id.to_text()).unwrap(), id);
        }
    }
}
