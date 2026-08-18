//! Content identity for immutable blob bytes.
//!
//! The normative specification is `docs/spec/blob-storage.md`.

use std::{error::Error, fmt, str::FromStr};

use sha2::{Digest, Sha256};

const EXTERNAL_PREFIX: &str = "sha256:";
const SHA256_HEX_BYTES: usize = 64;

/// SHA-256 of one exact immutable blob byte sequence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlobDigest([u8; 32]);

impl BlobDigest {
    /// Reconstitutes one stored digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the fixed digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hashes exact blob bytes.
    ///
    /// Blob admission separately rejects an empty byte sequence; hashing is a
    /// pure identity operation and therefore also supports verification code.
    pub fn digest(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl fmt::Display for BlobDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(EXTERNAL_PREFIX)?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for BlobDigest {
    type Err = BlobDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let encoded = value.strip_prefix(EXTERNAL_PREFIX).ok_or_else(|| {
            BlobDigestParseError::new(value, BlobDigestParseFailure::MissingSha256Prefix)
        })?;
        if encoded.len() != SHA256_HEX_BYTES {
            return Err(BlobDigestParseError::new(
                value,
                BlobDigestParseFailure::InvalidLength,
            ));
        }
        let mut bytes = [0_u8; 32];
        for (destination, pair) in bytes.iter_mut().zip(encoded.as_bytes().chunks_exact(2)) {
            let high = lowercase_hex_value(pair[0]).ok_or_else(|| {
                BlobDigestParseError::new(value, BlobDigestParseFailure::InvalidHex)
            })?;
            let low = lowercase_hex_value(pair[1]).ok_or_else(|| {
                BlobDigestParseError::new(value, BlobDigestParseFailure::InvalidHex)
            })?;
            *destination = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

fn lowercase_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Closed reason an external blob digest spelling was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobDigestParseFailure {
    /// The required `sha256:` algorithm tag was absent.
    MissingSha256Prefix,
    /// The hexadecimal payload was not exactly 64 bytes.
    InvalidLength,
    /// The payload contained a non-lowercase-hexadecimal byte.
    InvalidHex,
}

impl fmt::Display for BlobDigestParseFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSha256Prefix => formatter.write_str("blob digest lacks the sha256 prefix"),
            Self::InvalidLength => formatter.write_str("blob digest has the wrong length"),
            Self::InvalidHex => formatter.write_str("blob digest is not lowercase hexadecimal"),
        }
    }
}

/// Rejected external blob digest spelling and its typed failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobDigestParseError {
    rejected: String,
    failure: BlobDigestParseFailure,
}

impl BlobDigestParseError {
    fn new(rejected: &str, failure: BlobDigestParseFailure) -> Self {
        Self {
            rejected: rejected.to_owned(),
            failure,
        }
    }

    /// Returns the exact rejected spelling.
    pub fn rejected(&self) -> &str {
        &self.rejected
    }

    /// Returns the closed rejection reason.
    pub const fn failure(&self) -> BlobDigestParseFailure {
        self.failure
    }
}

impl fmt::Display for BlobDigestParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.failure.fmt(formatter)
    }
}

impl Error for BlobDigestParseError {}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "digest tests use explicit rejection expectations"
    )]

    use std::str::FromStr;

    use super::{BlobDigest, BlobDigestParseFailure};

    const ABC_SHA256: &str =
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn inv056_exact_bytes_have_one_tagged_lowercase_identity() {
        let first_producer = BlobDigest::digest(b"abc");
        let independently_owned_bytes = Vec::from(*b"abc");
        let second_producer = BlobDigest::digest(&independently_owned_bytes);

        assert_eq!(first_producer, second_producer);
        assert_eq!(first_producer.to_string(), ABC_SHA256);
        assert_eq!(BlobDigest::from_str(ABC_SHA256), Ok(first_producer));
    }

    #[test]
    fn external_identity_requires_the_sha256_tag() {
        let error = BlobDigest::from_str(&ABC_SHA256["sha256:".len()..]);

        let error = error.expect_err("a tagless digest must be rejected");
        assert_eq!(error.failure(), BlobDigestParseFailure::MissingSha256Prefix);
        assert_eq!(error.rejected(), &ABC_SHA256["sha256:".len()..]);
    }

    #[test]
    fn external_identity_requires_exact_digest_length() {
        let error = BlobDigest::from_str("sha256:00");

        let error = error.expect_err("a short digest must be rejected");
        assert_eq!(error.failure(), BlobDigestParseFailure::InvalidLength);
        assert_eq!(error.rejected(), "sha256:00");
    }

    #[test]
    fn external_identity_rejects_uppercase_hexadecimal() {
        let uppercase = ABC_SHA256.replace('b', "B");

        let error =
            BlobDigest::from_str(&uppercase).expect_err("uppercase hexadecimal must be rejected");
        assert_eq!(error.failure(), BlobDigestParseFailure::InvalidHex);
        assert_eq!(error.rejected(), uppercase);
    }
}
