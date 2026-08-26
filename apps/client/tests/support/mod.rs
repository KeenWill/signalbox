use signalboxd::{HubModelConfiguration, HubModelConfigurationError};

pub fn parse_model_configuration(
    content: &str,
) -> Result<HubModelConfiguration, HubModelConfigurationError> {
    let example = include_str!("../../../../config/signalboxd.example.toml");
    let (_, numeric_bounds_and_after) = example
        .split_once("[numeric_bounds]")
        .ok_or(HubModelConfigurationError::InvalidDocument)?;
    let (numeric_bounds, _) = numeric_bounds_and_after
        .split_once("\n[")
        .ok_or(HubModelConfigurationError::InvalidDocument)?;
    HubModelConfiguration::parse(&format!("{content}\n[numeric_bounds]{numeric_bounds}\n"))
}
