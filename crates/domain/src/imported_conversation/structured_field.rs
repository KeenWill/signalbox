//! Imported structured-field accessors and attestations for `docs/spec/conversation-import.md`.

use super::content::ImportedMediaSource;
use super::structured_value::ImportedSourceAttestation;
use super::structured_value::ImportedStructuredObjectMember;
use super::structured_value::ImportedStructuredValue;
use super::structured_value::ImportedText;
use std::error::Error;
use std::fmt;

/// Content-silent failure to read one consulted imported structured field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportedStructuredFieldError;

impl fmt::Display for ImportedStructuredFieldError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("imported structured field was duplicated or had an invalid shape")
    }
}

impl Error for ImportedStructuredFieldError {}

fn structured_field_error() -> ImportedStructuredFieldError {
    ImportedStructuredFieldError
}

/// Selects at most one exact-name member, rejecting a consulted duplicate.
pub fn unique_imported_structured_field<'members>(
    members: &'members [ImportedStructuredObjectMember],
    name: &str,
) -> Result<Option<&'members ImportedStructuredValue>, ImportedStructuredFieldError> {
    let mut found = None;
    for member in members {
        if member.name().as_str() == name {
            if found.is_some() {
                return Err(structured_field_error());
            }
            found = Some(member.value());
        }
    }
    Ok(found)
}

/// Reads omitted, null, or string field evidence without collapsing absence.
pub fn imported_text_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedText>, ImportedStructuredFieldError> {
    match unique_imported_structured_field(members, name)? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(ImportedStructuredValue::String(value)) => {
            Ok(ImportedSourceAttestation::Attested(value.clone()))
        }
        Some(_) => Err(structured_field_error()),
    }
}

/// Reads omitted, null, or Boolean field evidence without collapsing absence.
pub fn imported_bool_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<bool>, ImportedStructuredFieldError> {
    match unique_imported_structured_field(members, name)? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(ImportedStructuredValue::Boolean(value)) => {
            Ok(ImportedSourceAttestation::Attested(*value))
        }
        Some(_) => Err(structured_field_error()),
    }
}

/// Reads omitted, null, or arbitrary structured field evidence.
pub fn imported_structured_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedStructuredValue>, ImportedStructuredFieldError> {
    match unique_imported_structured_field(members, name)? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(value) => Ok(ImportedSourceAttestation::Attested(value.clone())),
    }
}

/// Reads structured field evidence whose attested value must be a string.
pub fn imported_string_structured_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedStructuredValue>, ImportedStructuredFieldError> {
    match unique_imported_structured_field(members, name)? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(value @ ImportedStructuredValue::String(_)) => {
            Ok(ImportedSourceAttestation::Attested(value.clone()))
        }
        Some(_) => Err(structured_field_error()),
    }
}

pub(super) fn projected_text_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedText>, ()> {
    imported_text_attestation(members, name).map_err(|_| ())
}

pub(super) fn projected_bool_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<bool>, ()> {
    imported_bool_attestation(members, name).map_err(|_| ())
}

pub(super) fn projected_structured_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedStructuredValue>, ()> {
    imported_structured_attestation(members, name).map_err(|_| ())
}

pub(super) fn projected_string_structured_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedStructuredValue>, ()> {
    imported_string_structured_attestation(members, name).map_err(|_| ())
}

pub(super) fn projected_media_source_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedMediaSource>, ()> {
    match unique_structured_field(members, name)? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(ImportedStructuredValue::Object(source)) => Ok(ImportedSourceAttestation::Attested(
            ImportedMediaSource::new(
                projected_text_attestation(source, "type")?,
                projected_text_attestation(source, "media_type")?,
                projected_text_attestation(source, "data")?,
            ),
        )),
        Some(_) => Err(()),
    }
}

pub(super) fn unique_structured_field<'members>(
    members: &'members [ImportedStructuredObjectMember],
    name: &str,
) -> Result<Option<&'members ImportedStructuredValue>, ()> {
    unique_imported_structured_field(members, name).map_err(|_| ())
}
