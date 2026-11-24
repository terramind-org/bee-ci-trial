//! Conversion of one message between proto3-JSON and protobuf binary.

use std::fmt;

use prost::Message;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Why a message could not be converted.
#[derive(Debug)]
pub enum TranscodeError {
    /// The JSON was not a valid proto3-JSON encoding of the message.
    Json(serde_json::Error),
    /// The bytes were not a valid protobuf encoding of the message.
    Decode(prost::DecodeError),
}

impl fmt::Display for TranscodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(e) => write!(f, "invalid JSON message: {e}"),
            Self::Decode(e) => write!(f, "invalid protobuf message: {e}"),
        }
    }
}

impl std::error::Error for TranscodeError {}

/// A function that converts one encoded message to another encoding.
pub type TranscodeFn = fn(&[u8]) -> Result<Vec<u8>, TranscodeError>;

/// Parses proto3-JSON into `T` and returns its protobuf encoding. An empty or all-whitespace
/// body is read as `{}`. Unknown JSON fields are ignored (forward compatibility).
pub fn json_to_proto<T>(json: &[u8]) -> Result<Vec<u8>, TranscodeError>
where
    T: Message + Default + DeserializeOwned,
{
    if json.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default().encode_to_vec());
    }
    let message: T = serde_json::from_slice(json).map_err(TranscodeError::Json)?;
    Ok(message.encode_to_vec())
}

/// Decodes protobuf bytes as `T` and returns its proto3-JSON encoding.
pub fn proto_to_json<T>(bytes: &[u8]) -> Result<Vec<u8>, TranscodeError>
where
    T: Message + Default + Serialize,
{
    let message = T::decode(bytes).map_err(TranscodeError::Decode)?;
    serde_json::to_vec(&message).map_err(TranscodeError::Json)
}
