use super::error::HubModelConfigurationError;
use signalbox_process_protocol::{
    MAX_MODEL_ALIAS_CATALOG_ENTRIES, MAX_MODEL_CAPABILITY_CATALOG_ENTRIES,
};
use std::{collections::HashMap, sync::Arc};
use toml_edit::{Item, Table};
use uuid::Uuid;

pub(super) fn validate_alias_count(count: usize) -> Result<(), HubModelConfigurationError> {
    if count > MAX_MODEL_ALIAS_CATALOG_ENTRIES {
        Err(HubModelConfigurationError::TooManyAliases)
    } else {
        Ok(())
    }
}

pub(super) fn validate_model_count(count: usize) -> Result<(), HubModelConfigurationError> {
    if count > MAX_MODEL_CAPABILITY_CATALOG_ENTRIES {
        Err(HubModelConfigurationError::TooManyModels)
    } else {
        Ok(())
    }
}

pub(crate) fn validated_name(value: &str) -> Result<Arc<str>, HubModelConfigurationError> {
    if value.is_empty() || value.trim() != value || value.contains('\0') {
        Err(HubModelConfigurationError::InvalidField)
    } else {
        Ok(Arc::from(value))
    }
}

pub(crate) fn reject_unknown_fields(
    table: &Table,
    allowed: &[&str],
) -> Result<(), HubModelConfigurationError> {
    if table.iter().any(|(key, _)| !allowed.contains(&key)) {
        Err(HubModelConfigurationError::UnknownField)
    } else {
        Ok(())
    }
}

pub(crate) fn required_string<'a>(
    table: &'a Table,
    key: &str,
) -> Result<&'a str, HubModelConfigurationError> {
    table
        .get(key)
        .and_then(|item| item.as_str())
        .ok_or(HubModelConfigurationError::InvalidField)
}

pub(super) fn required_uuid(table: &Table, key: &str) -> Result<Uuid, HubModelConfigurationError> {
    Uuid::parse_str(required_string(table, key)?)
        .map_err(|_| HubModelConfigurationError::InvalidIdentity)
}

pub(super) fn required_positive_u32(
    table: &Table,
    key: &str,
) -> Result<u32, HubModelConfigurationError> {
    let value = table
        .get(key)
        .and_then(|item| item.as_integer())
        .ok_or(HubModelConfigurationError::InvalidField)?;
    let value = u32::try_from(value).map_err(|_| HubModelConfigurationError::InvalidLimit)?;
    if value == 0 {
        Err(HubModelConfigurationError::InvalidLimit)
    } else {
        Ok(value)
    }
}

pub(super) fn parse_positive_u32_inline_map(
    item: Option<&Item>,
) -> Result<HashMap<String, u32>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(HashMap::new());
    };
    let table = item
        .as_inline_table()
        .ok_or(HubModelConfigurationError::InvalidCodexCliConfiguration)?;
    table
        .iter()
        .map(|(target, value)| {
            validated_name(target)
                .map_err(|_| HubModelConfigurationError::InvalidCodexCliConfiguration)?;
            let value = value
                .as_integer()
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or(HubModelConfigurationError::InvalidCodexCliConfiguration)?;
            Ok((target.to_string(), value))
        })
        .collect()
}
