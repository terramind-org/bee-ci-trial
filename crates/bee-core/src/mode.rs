//! Tree entry modes (SPEC §4.2).

use crate::error::{BeeError, Result, Subcode};

/// The mode of a tree entry or delta put.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// `1`: regular file.
    File,
    /// `2`: executable file.
    Executable,
    /// `3`: symbolic link; the blob holds the target.
    Symlink,
    /// `4`: directory.
    Tree,
    /// `5`: submodule pointer; the id is a foreign git id.
    Gitlink,
}

impl Mode {
    /// Every mode, in numeric order.
    pub const ALL: [Mode; 5] = [
        Mode::File,
        Mode::Executable,
        Mode::Symlink,
        Mode::Tree,
        Mode::Gitlink,
    ];

    /// The encoded value.
    pub fn as_u64(self) -> u64 {
        match self {
            Mode::File => 1,
            Mode::Executable => 2,
            Mode::Symlink => 3,
            Mode::Tree => 4,
            Mode::Gitlink => 5,
        }
    }

    /// Parses an encoded value.
    pub fn from_u64(value: u64) -> Result<Mode> {
        match value {
            1 => Ok(Mode::File),
            2 => Ok(Mode::Executable),
            3 => Ok(Mode::Symlink),
            4 => Ok(Mode::Tree),
            5 => Ok(Mode::Gitlink),
            _ => Err(BeeError::invalid(
                Subcode::InvalidMode,
                format!("unknown mode {value}"),
            )),
        }
    }

    /// True for modes whose id is a blob: file, executable, symlink.
    pub fn is_blob(self) -> bool {
        matches!(self, Mode::File | Mode::Executable | Mode::Symlink)
    }

    /// True for every mode except [`Mode::Tree`]; these are the "files" a delta records.
    pub fn is_leaf(self) -> bool {
        self != Mode::Tree
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_values_round_trip() {
        for (i, m) in Mode::ALL.iter().enumerate() {
            assert_eq!(m.as_u64(), i as u64 + 1);
            assert_eq!(Mode::from_u64(m.as_u64()).unwrap(), *m);
        }
        assert!(
            Mode::from_u64(0)
                .unwrap_err()
                .is_invalid(Subcode::InvalidMode)
        );
        assert!(Mode::from_u64(6).is_err());
    }

    #[test]
    fn mode_classes() {
        assert!(Mode::File.is_blob() && Mode::Executable.is_blob() && Mode::Symlink.is_blob());
        assert!(!Mode::Tree.is_blob() && !Mode::Gitlink.is_blob());
        assert!(Mode::Gitlink.is_leaf());
        assert!(!Mode::Tree.is_leaf());
    }
}
