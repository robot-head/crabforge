//! Git object identifiers.

use std::{fmt, str::FromStr};

/// A git object id: 20 raw bytes, rendered as 40 lowercase hex characters.
///
/// Stored raw rather than as a `String` so equality and hashing are cheap —
/// the object cache and packfile harvest compare millions of these.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid([u8; 20]);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidOid {
    #[error("object id must be 40 hex characters, got {0}")]
    Length(usize),
    #[error("object id contains a non-hex character")]
    NotHex,
}

impl Oid {
    pub const HEX_LEN: usize = 40;

    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// The all-zeroes id, which git uses on the wire to mean "no such object"
    /// — a ref creation has this as its old value, a deletion as its new one.
    pub fn zero() -> Self {
        Self([0u8; 20])
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 20]
    }

    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(Self::HEX_LEN);
        for byte in self.0 {
            use fmt::Write as _;
            let _ = write!(s, "{byte:02x}");
        }
        s
    }
}

impl FromStr for Oid {
    type Err = InvalidOid;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != Self::HEX_LEN {
            return Err(InvalidOid::Length(s.len()));
        }
        let mut out = [0u8; 20];
        for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
            let hi = hex_val(chunk[0]).ok_or(InvalidOid::NotHex)?;
            let lo = hex_val(chunk[1]).ok_or(InvalidOid::NotHex)?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.to_hex())
    }
}

impl serde::Serialize for Oid {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for Oid {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(d)?;
        Oid::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    const SAMPLE: &str = "e83c5163316f89bfbde7d9ab23ca2e25604af290";

    #[test]
    fn hex_round_trips() {
        let oid = Oid::from_str(SAMPLE).unwrap();
        check!(oid.to_hex() == SAMPLE);
    }

    #[test]
    fn uppercase_hex_parses_and_normalizes_to_lowercase() {
        let oid = Oid::from_str(&SAMPLE.to_uppercase()).unwrap();
        check!(oid.to_hex() == SAMPLE);
    }

    #[test]
    fn wrong_length_is_rejected() {
        assert!(let Err(InvalidOid::Length(4)) = Oid::from_str("dead"));
    }

    #[test]
    fn non_hex_is_rejected() {
        let bad = "z".repeat(40);
        assert!(let Err(InvalidOid::NotHex) = Oid::from_str(&bad));
    }

    #[test]
    fn zero_oid_is_recognized() {
        check!(Oid::from_str(&"0".repeat(40)).unwrap().is_zero());
        check!(!Oid::from_str(SAMPLE).unwrap().is_zero());
    }

    #[test]
    fn serde_uses_the_hex_spelling() {
        let oid = Oid::from_str(SAMPLE).unwrap();
        let json = serde_json::to_string(&oid).unwrap();
        check!(json == format!("\"{SAMPLE}\""));
        check!(serde_json::from_str::<Oid>(&json).unwrap() == oid);
    }
}
