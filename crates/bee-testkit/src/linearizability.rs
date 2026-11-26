//! Stable capture files for failed ref linearizability checks.
//!
//! The checker itself lives in conformance because it is exercised against a running server.
//! This lower-layer format keeps the recorded checker input portable and independently readable.

#![allow(missing_docs)] // Field names are the versioned JSON schema documented by `Capture`.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A complete checker input captured from one randomized run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    /// Format version, allowing future readers to reject incompatible data explicitly.
    pub format: u32,
    /// Random seed selected for the run.
    pub seed: u64,
    /// Initial refs: name, object id text, version.
    pub initial: Vec<InitialRef>,
    /// Final landed-index oracle used by the checker.
    pub results: Vec<LandedResult>,
    /// First-parent oracle used by the checker.
    pub parents: Vec<Parent>,
    /// Operations in the exact order consumed by the checker.
    pub operations: Vec<CapturedOperation>,
    /// Fault points and hit indices that fired during the run.
    pub faults: Vec<FaultHit>,
    /// Store state observed after the run.  Absent in captures written before this field
    /// existed, so older fixtures remain valid checker inputs.
    #[serde(default)]
    pub truth: GroundTruth,
}

/// Store facts that distinguish a recorder/model failure from a bad store transition.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroundTruth {
    pub refs: Vec<RefTruth>,
    pub land_responses: Vec<LandResponse>,
}

/// Durable state for one ref, read after all clients have stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefTruth {
    pub name: String,
    pub log: Vec<RefLogTruth>,
    pub landed: Vec<LandedTruth>,
}

/// One durable ref-log entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefLogTruth {
    pub version: u64,
    pub target: Option<String>,
    pub landed_change_ids: Vec<String>,
}

/// One durable landed-index row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandedTruth {
    pub change_id: String,
    pub source: String,
    pub result: String,
    pub version: u64,
}

/// A successful Land response, duplicated from the history to make its version explicit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandResponse {
    pub name: String,
    pub version: u64,
    pub results: Vec<String>,
}

/// Initial state of one ref.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialRef {
    pub name: String,
    pub target: String,
    pub version: u64,
}

/// An entry in the final landed index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandedResult {
    pub name: String,
    pub change_id: String,
    pub result: String,
}

/// A recorded changeset parent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    pub child: String,
    pub parent: String,
}

/// One operation with the logical ordering used by the checker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedOperation {
    pub client: usize,
    pub call: u64,
    pub ret: Option<u64>,
    pub input: CapturedInput,
    pub output: Option<CapturedOutput>,
}

/// A ref operation's arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapturedInput {
    Read {
        name: String,
    },
    Cas {
        name: String,
        expected: u64,
        target: String,
    },
    Land {
        name: String,
        stack: Vec<StackMember>,
    },
}

/// One member of a land stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackMember {
    pub change_id: String,
    pub source: String,
}

/// A definite operation result. `None` records a lost or otherwise unknown response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapturedOutput {
    Read {
        target: Option<String>,
        version: u64,
    },
    CasOk {
        version: u64,
    },
    CasMismatch {
        current: u64,
    },
    Landed {
        version: u64,
        results: Vec<String>,
        skipped: bool,
    },
    AlreadyLanded,
}

/// One injected fault that fired during the run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultHit {
    pub point: String,
    pub hit: u64,
}

/// Encodes a capture as canonical pretty JSON (with a trailing newline).
pub fn encode(capture: &Capture) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(capture)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Decodes a capture from bytes.
pub fn decode(bytes: &[u8]) -> io::Result<Capture> {
    let capture: Capture =
        serde_json::from_slice(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if capture.format != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported capture format",
        ));
    }
    Ok(capture)
}

/// Writes a capture file.
pub fn write(path: &Path, capture: &Capture) -> io::Result<()> {
    fs::write(path, encode(capture)?)
}

/// Reads a capture file back into the checker-input capture type.
pub fn read(path: &Path) -> io::Result<Capture> {
    decode(&fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_round_trips_without_losing_checker_input() {
        let capture = Capture {
            format: 1,
            seed: 23,
            initial: vec![InitialRef {
                name: "main".into(),
                target: "c_00".into(),
                version: 1,
            }],
            results: vec![],
            parents: vec![],
            operations: vec![CapturedOperation {
                client: 2,
                call: 4,
                ret: None,
                input: CapturedInput::Cas {
                    name: "ws/lin-0".into(),
                    expected: 1,
                    target: "c_01".into(),
                },
                output: None,
            }],
            faults: vec![FaultHit {
                point: "meta.update_ref".into(),
                hit: 7,
            }],
            truth: GroundTruth::default(),
        };
        assert_eq!(decode(&encode(&capture).unwrap()).unwrap(), capture);
    }
}
