//! Content identity for immutable blob bytes.
//!
//! The normative specification is `docs/spec/blob-storage.md`.

use std::{error::Error, fmt, num::NonZeroU32, str::FromStr, sync::Arc};

use sha2::{Digest, Sha256};

const EXTERNAL_PREFIX: &str = "sha256:";
const SHA256_HEX_BYTES: usize = 64;

const MAX_BLOB_DERIVATION_INPUTS: usize = 16;
const MAX_BLOB_DERIVATION_OUTPUTS: usize = 16;
const MAX_TRANSFORMATION_NAME_BYTES: usize = 64;
const MAX_TRANSFORMATION_PARAMETERS_BYTES: usize = 4096;
const DETERMINISTIC_DERIVATION_DOMAIN: &[u8] = b"signalbox.blob-derivation.v1\0";

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
        write!(formatter, "{EXTERNAL_PREFIX}{}", hex::encode(self.0))
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
        if !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(BlobDigestParseError::new(
                value,
                BlobDigestParseFailure::InvalidHex,
            ));
        }
        let mut bytes = [0_u8; 32];
        hex::decode_to_slice(encoded, &mut bytes)
            .map_err(|_| BlobDigestParseError::new(value, BlobDigestParseFailure::InvalidHex))?;
        Ok(Self(bytes))
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

/// Stable lowercase-ASCII name of one transformation procedure.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlobTransformationName(Arc<str>);

impl BlobTransformationName {
    /// Admits `[a-z][a-z0-9_.-]{0,63}` as a procedure name.
    pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self, BlobTransformationError> {
        let value = value.into();
        let mut bytes = value.bytes();
        let valid_first = bytes.next().is_some_and(|byte| byte.is_ascii_lowercase());
        let valid_rest = bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        });
        if valid_first && valid_rest && value.len() <= MAX_TRANSFORMATION_NAME_BYTES {
            Ok(Self(value))
        } else {
            Err(BlobTransformationError::InvalidName)
        }
    }

    /// Borrows the exact stable name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Versioned transformation definition and exact canonical parameters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobTransformation {
    name: BlobTransformationName,
    version: NonZeroU32,
    parameters_json: Box<str>,
}

impl BlobTransformation {
    /// Constructs a definition using compact JSON with deterministically sorted object keys.
    pub fn try_new(
        name: BlobTransformationName,
        version: u32,
        parameters: &serde_json::Value,
    ) -> Result<Self, BlobTransformationError> {
        let version = NonZeroU32::new(version).ok_or(BlobTransformationError::ZeroVersion)?;
        let parameters_json = serde_json::to_string(parameters)
            .map_err(|_| BlobTransformationError::InvalidParameters)?
            .into_boxed_str();
        if parameters_json.len() > MAX_TRANSFORMATION_PARAMETERS_BYTES {
            return Err(BlobTransformationError::ParametersTooLarge);
        }
        Ok(Self {
            name,
            version,
            parameters_json,
        })
    }

    /// Returns the stable procedure name.
    pub const fn name(&self) -> &BlobTransformationName {
        &self.name
    }

    /// Returns the positive procedure version.
    pub const fn version(&self) -> NonZeroU32 {
        self.version
    }

    /// Borrows the canonical compact JSON parameters.
    pub fn parameters_json(&self) -> &str {
        &self.parameters_json
    }
}

/// Closed transformation-definition rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobTransformationError {
    /// The stable procedure name violated its closed grammar.
    InvalidName,
    /// Transformation versions are positive.
    ZeroVersion,
    /// Parameters could not be encoded as JSON.
    InvalidParameters,
    /// Canonical parameters crossed their durable bound.
    ParametersTooLarge,
}

impl fmt::Display for BlobTransformationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidName => "blob transformation name is invalid",
            Self::ZeroVersion => "blob transformation version must be positive",
            Self::InvalidParameters => "blob transformation parameters are invalid",
            Self::ParametersTooLarge => "blob transformation parameters exceed the bound",
        })
    }
}

impl Error for BlobTransformationError {}

/// Exact producer provenance for one immutable derivation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobDerivationProducer {
    /// Reusable procedure whose implementation bytes have one digest.
    Deterministic { implementation: BlobDigest },
    /// Nondeterministic execution identified exactly and tied to its implementation.
    Executed {
        execution_id: uuid::Uuid,
        implementation: BlobDigest,
    },
    /// Output derived by one durable model call.
    ModelDerived { model_call: crate::ModelCallId },
}

/// Content/procedure cache identity for a deterministic derivation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeterministicBlobDerivationKey(BlobDigest);

impl DeterministicBlobDerivationKey {
    /// Derives a reusable key from bounded ordered inputs and exact procedure provenance.
    pub fn try_derive(
        inputs: &[BlobDigest],
        transformation: &BlobTransformation,
        implementation: BlobDigest,
    ) -> Result<Self, BlobDerivationError> {
        if inputs.is_empty() {
            return Err(BlobDerivationError::EmptyInputs);
        }
        if inputs.len() > MAX_BLOB_DERIVATION_INPUTS {
            return Err(BlobDerivationError::TooManyInputs);
        }
        Ok(derive_deterministic_key(
            inputs,
            transformation,
            implementation,
        ))
    }

    /// Returns the SHA-256 key bytes.
    pub const fn digest(self) -> BlobDigest {
        self.0
    }
}

/// Immutable relationship between input and output blob identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobDerivation {
    id: crate::BlobDerivationId,
    inputs: Box<[BlobDigest]>,
    transformation: BlobTransformation,
    producer: BlobDerivationProducer,
    outputs: Box<[BlobDigest]>,
}

impl BlobDerivation {
    /// Admits one bounded derivation fact without interpreting its outputs.
    pub fn try_new(
        id: crate::BlobDerivationId,
        inputs: impl Into<Box<[BlobDigest]>>,
        transformation: BlobTransformation,
        producer: BlobDerivationProducer,
        outputs: impl Into<Box<[BlobDigest]>>,
    ) -> Result<Self, BlobDerivationError> {
        let inputs = inputs.into();
        let outputs = outputs.into();
        if inputs.is_empty() {
            return Err(BlobDerivationError::EmptyInputs);
        }
        if inputs.len() > MAX_BLOB_DERIVATION_INPUTS {
            return Err(BlobDerivationError::TooManyInputs);
        }
        if outputs.is_empty() {
            return Err(BlobDerivationError::EmptyOutputs);
        }
        if outputs.len() > MAX_BLOB_DERIVATION_OUTPUTS {
            return Err(BlobDerivationError::TooManyOutputs);
        }
        Ok(Self {
            id,
            inputs,
            transformation,
            producer,
            outputs,
        })
    }

    /// Returns the durable fact identity.
    pub const fn id(&self) -> crate::BlobDerivationId {
        self.id
    }

    /// Returns ordered input identities.
    pub fn inputs(&self) -> &[BlobDigest] {
        &self.inputs
    }

    /// Returns the exact versioned procedure and parameters.
    pub const fn transformation(&self) -> &BlobTransformation {
        &self.transformation
    }

    /// Returns the exact producer class and provenance.
    pub const fn producer(&self) -> BlobDerivationProducer {
        self.producer
    }

    /// Returns ordered output identities.
    pub fn outputs(&self) -> &[BlobDigest] {
        &self.outputs
    }

    /// Derives the reusable key only for deterministic producer provenance.
    pub fn deterministic_key(&self) -> Option<DeterministicBlobDerivationKey> {
        let BlobDerivationProducer::Deterministic { implementation } = self.producer else {
            return None;
        };
        DeterministicBlobDerivationKey::try_derive(
            &self.inputs,
            &self.transformation,
            implementation,
        )
        .ok()
    }
}

/// Closed derivation-shape rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobDerivationError {
    EmptyInputs,
    TooManyInputs,
    EmptyOutputs,
    TooManyOutputs,
}

impl fmt::Display for BlobDerivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyInputs => "blob derivation has no inputs",
            Self::TooManyInputs => "blob derivation has too many inputs",
            Self::EmptyOutputs => "blob derivation has no outputs",
            Self::TooManyOutputs => "blob derivation has too many outputs",
        })
    }
}

impl Error for BlobDerivationError {}

fn derive_deterministic_key(
    inputs: &[BlobDigest],
    transformation: &BlobTransformation,
    implementation: BlobDigest,
) -> DeterministicBlobDerivationKey {
    let mut digest = Sha256::new();
    digest.update(DETERMINISTIC_DERIVATION_DOMAIN);
    digest.update(
        u64::try_from(inputs.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for input in inputs {
        digest.update(input.as_bytes());
    }
    update_length_prefixed(&mut digest, transformation.name().as_str().as_bytes());
    digest.update(transformation.version().get().to_be_bytes());
    update_length_prefixed(&mut digest, transformation.parameters_json().as_bytes());
    digest.update(implementation.as_bytes());
    DeterministicBlobDerivationKey(BlobDigest::from_bytes(digest.finalize().into()))
}

fn update_length_prefixed(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "digest tests use explicit rejection expectations"
    )]

    use std::str::FromStr;

    use super::{
        BlobDerivation, BlobDerivationProducer, BlobDigest, BlobDigestParseFailure,
        BlobTransformation, BlobTransformationName, DeterministicBlobDerivationKey,
    };

    const ABC_SHA256: &str =
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn exact_bytes_have_one_tagged_lowercase_identity() {
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

    #[test]
    /// deterministic reuse is fixed by inputs, procedure, parameters, and implementation.
    fn deterministic_derivation_keys_cover_exact_procedure_provenance() {
        let transformation = BlobTransformation::try_new(
            BlobTransformationName::try_new("image.thumbnail")
                .expect("the fixture procedure name is valid"),
            1,
            &serde_json::json!({"edge_px": 256}),
        )
        .expect("the fixture transformation is valid");
        let input = BlobDigest::digest(b"input");
        let implementation = BlobDigest::digest(b"implementation-v1");
        let first = BlobDerivation::try_new(
            crate::BlobDerivationId::from_uuid(uuid::Uuid::from_u128(1)),
            [input],
            transformation.clone(),
            BlobDerivationProducer::Deterministic { implementation },
            [BlobDigest::digest(b"output")],
        )
        .expect("the first fixture is valid");
        let replay = BlobDerivation::try_new(
            crate::BlobDerivationId::from_uuid(uuid::Uuid::from_u128(2)),
            [input],
            transformation,
            BlobDerivationProducer::Deterministic { implementation },
            [BlobDigest::digest(b"independently-produced-output")],
        )
        .expect("the replay fixture is valid");
        let changed_implementation = BlobDerivation::try_new(
            crate::BlobDerivationId::from_uuid(uuid::Uuid::from_u128(3)),
            [input],
            replay.transformation().clone(),
            BlobDerivationProducer::Deterministic {
                implementation: BlobDigest::digest(b"implementation-v2"),
            },
            [BlobDigest::digest(b"changed-implementation-output")],
        )
        .expect("the changed implementation fixture is valid");

        let expected = first
            .deterministic_key()
            .expect("a deterministic producer has a key");
        let changed_input = DeterministicBlobDerivationKey::try_derive(
            &[BlobDigest::digest(b"another-input")],
            first.transformation(),
            implementation,
        )
        .expect("the changed input has a deterministic key");
        let changed_name = BlobTransformation::try_new(
            BlobTransformationName::try_new("image.preview")
                .expect("the changed procedure name is valid"),
            1,
            &serde_json::json!({"edge_px": 256}),
        )
        .expect("the changed procedure is valid");
        let changed_name =
            DeterministicBlobDerivationKey::try_derive(&[input], &changed_name, implementation)
                .expect("the changed procedure has a deterministic key");
        let changed_version = BlobTransformation::try_new(
            BlobTransformationName::try_new("image.thumbnail")
                .expect("the fixture procedure name is valid"),
            2,
            &serde_json::json!({"edge_px": 256}),
        )
        .expect("the changed version is valid");
        let changed_version =
            DeterministicBlobDerivationKey::try_derive(&[input], &changed_version, implementation)
                .expect("the changed version has a deterministic key");
        let changed_parameters = BlobTransformation::try_new(
            BlobTransformationName::try_new("image.thumbnail")
                .expect("the fixture procedure name is valid"),
            1,
            &serde_json::json!({"edge_px": 512}),
        )
        .expect("the changed parameters are valid");
        let changed_parameters = DeterministicBlobDerivationKey::try_derive(
            &[input],
            &changed_parameters,
            implementation,
        )
        .expect("the changed parameters have a deterministic key");
        assert_eq!(replay.deterministic_key(), Some(expected));
        assert_ne!(changed_input, expected);
        assert_ne!(changed_name, expected);
        assert_ne!(changed_version, expected);
        assert_ne!(changed_parameters, expected);
        assert_ne!(changed_implementation.deterministic_key(), Some(expected));
    }
}
