use super::{
    error::HubModelConfigurationError,
    model_routing::ModelBillingRates,
    toml_scalars::{required_string, validated_name},
};
use rust_decimal::Decimal;
use signalbox_process_protocol::MAX_RATE_VERSION_UTF8_BYTES;
use std::sync::Arc;
use toml_edit::Table;

pub(super) fn parse_model_billing_rates(
    model: &Table,
) -> Result<Option<ModelBillingRates>, HubModelConfigurationError> {
    const RATE_FIELDS: [&str; 5] = [
        "rate_version",
        "input_usd_per_million_tokens",
        "output_usd_per_million_tokens",
        "cache_creation_input_usd_per_million_tokens",
        "cache_read_input_usd_per_million_tokens",
    ];
    if RATE_FIELDS.iter().all(|field| model.get(field).is_none()) {
        return Ok(None);
    }
    if RATE_FIELDS.iter().any(|field| model.get(field).is_none()) {
        return Err(HubModelConfigurationError::IncompleteBillingRates);
    }
    Ok(Some(ModelBillingRates {
        version: validated_rate_version(required_string(model, "rate_version")?)?,
        input: required_billing_rate(model, "input_usd_per_million_tokens")?,
        output: required_billing_rate(model, "output_usd_per_million_tokens")?,
        cache_creation_input: required_billing_rate(
            model,
            "cache_creation_input_usd_per_million_tokens",
        )?,
        cache_read_input: required_billing_rate(model, "cache_read_input_usd_per_million_tokens")?,
    }))
}

fn required_billing_rate(
    model: &Table,
    field: &str,
) -> Result<Decimal, HubModelConfigurationError> {
    let rate = Decimal::from_str_exact(required_string(model, field)?)
        .map_err(|_| HubModelConfigurationError::InvalidBillingRate)?;
    if rate.is_sign_negative() {
        Err(HubModelConfigurationError::InvalidBillingRate)
    } else {
        Ok(rate.normalize())
    }
}

fn validated_rate_version(value: &str) -> Result<Arc<str>, HubModelConfigurationError> {
    let version = validated_name(value)?;
    if version.len() > MAX_RATE_VERSION_UTF8_BYTES {
        Err(HubModelConfigurationError::InvalidBillingRate)
    } else {
        Ok(version)
    }
}

pub(super) fn fold_reported_cost(axes: [(Option<u128>, Decimal); 4]) -> Option<Decimal> {
    const TOKENS_PER_MILLION: u64 = 1_000_000;
    let mut amount = Decimal::ZERO;
    let mut reported = false;
    for (tokens, rate) in axes {
        let Some(tokens) = tokens else {
            continue;
        };
        reported = true;
        let numerator = exact_rate_token_product(rate, tokens)?;
        let axis_cost = numerator.checked_div(Decimal::from(TOKENS_PER_MILLION))?;
        if axis_cost.checked_mul(Decimal::from(TOKENS_PER_MILLION))? != numerator {
            return None;
        }
        let next_amount = amount.checked_add(axis_cost)?;
        if next_amount.checked_sub(amount)? != axis_cost
            || next_amount.checked_sub(axis_cost)? != amount
        {
            return None;
        }
        amount = next_amount;
    }
    reported.then(|| amount.normalize())
}

fn exact_rate_token_product(rate: Decimal, tokens: u128) -> Option<Decimal> {
    if tokens > u128::try_from(Decimal::MAX.mantissa()).ok()? {
        return None;
    }
    let product = rate.checked_mul(Decimal::from(tokens))?;
    let scale_loss = rate.scale().checked_sub(product.scale())?;
    if scale_loss == 0 {
        return Some(product);
    }
    let mut rate_mantissa = u128::try_from(rate.mantissa()).ok()?;
    let mut token_mantissa = tokens;
    for _ in 0..scale_loss {
        divide_product_factor(&mut rate_mantissa, &mut token_mantissa, 2)?;
        divide_product_factor(&mut rate_mantissa, &mut token_mantissa, 5)?;
    }
    let exact_mantissa = rate_mantissa.checked_mul(token_mantissa)?;
    (u128::try_from(product.mantissa()).ok()? == exact_mantissa).then_some(product)
}

fn divide_product_factor(left: &mut u128, right: &mut u128, factor: u128) -> Option<()> {
    if left.is_multiple_of(factor) {
        *left /= factor;
        Some(())
    } else if right.is_multiple_of(factor) {
        *right /= factor;
        Some(())
    } else {
        None
    }
}
