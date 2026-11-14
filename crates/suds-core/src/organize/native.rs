//! The lossless JSON form of a native path.
//!
//! A Unix path is a sequence of bytes that need not be valid UTF-8, so a plan
//! and the journal record each path's bytes in hexadecimal beside a display
//! string. Only the bytes name a file. Reading a document checks that the
//! display string matches the bytes, so an edited display string cannot
//! misdescribe the path that an operation acts on.

use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A path in its native form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "NativePathRepr", try_from = "NativePathRepr")]
pub struct NativePath(PathBuf);

#[derive(Serialize, Deserialize)]
struct NativePathRepr {
    /// The path with each invalid UTF-8 sequence replaced, for a person.
    display: String,
    /// The path's bytes in lowercase hexadecimal, which name the file.
    unix_bytes: String,
}

impl NativePath {
    pub fn new(path: PathBuf) -> Self {
        NativePath(path)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The path with each invalid UTF-8 sequence replaced, for display only.
    pub fn display(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl From<NativePath> for NativePathRepr {
    fn from(path: NativePath) -> Self {
        NativePathRepr {
            display: path.display(),
            unix_bytes: encode_hex(path.0.as_os_str().as_bytes()),
        }
    }
}

impl TryFrom<NativePathRepr> for NativePath {
    type Error = NativePathError;

    fn try_from(repr: NativePathRepr) -> Result<Self, Self::Error> {
        let bytes = decode_hex(&repr.unix_bytes).ok_or(NativePathError::InvalidBytes)?;
        let path = NativePath(PathBuf::from(OsString::from_vec(bytes)));
        if path.display() != repr.display {
            return Err(NativePathError::DisplayMismatch);
        }
        Ok(path)
    }
}

/// The reason that Suds cannot read a document's path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePathError {
    InvalidBytes,
    DisplayMismatch,
}

impl fmt::Display for NativePathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NativePathError::InvalidBytes => {
                write!(f, "a path's unix_bytes field is not lowercase hexadecimal")
            }
            NativePathError::DisplayMismatch => {
                write!(
                    f,
                    "a path's display field does not match its unix_bytes field"
                )
            }
        }
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let mut bytes = Vec::with_capacity(pairs.len());
    for [high, low] in pairs {
        bytes.push(hex_value(*high)? << 4 | hex_value(*low)?);
    }
    Some(bytes)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    use serde_json::json;

    use super::NativePath;

    #[test]
    fn round_trips_bytes_that_are_not_utf8() {
        let path = NativePath::new(PathBuf::from(OsStr::from_bytes(b"/tmp/caf\xe9")));
        let value = serde_json::to_value(&path).unwrap();
        assert_eq!(
            value,
            json!({ "display": "/tmp/caf\u{fffd}", "unix_bytes": "2f746d702f636166e9" })
        );
        let parsed: NativePath = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, path);
    }

    #[test]
    fn rejects_an_edited_display_string_and_malformed_bytes() {
        let edited = json!({ "display": "/etc/passwd", "unix_bytes": "2f746d70" });
        assert!(serde_json::from_value::<NativePath>(edited).is_err());
        let odd = json!({ "display": "/", "unix_bytes": "2" });
        assert!(serde_json::from_value::<NativePath>(odd).is_err());
        let upper = json!({ "display": "/", "unix_bytes": "2F" });
        assert!(serde_json::from_value::<NativePath>(upper).is_err());
    }
}
