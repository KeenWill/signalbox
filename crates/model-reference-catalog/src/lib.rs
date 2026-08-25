//! Non-routable, provenance-backed model identities and historical API rates.
//!
//! This crate deliberately has no dependency on Signalbox's domain, runtime,
//! provider-runtime, or persistence crates. Its embedded data is retrospective
//! reference material only; loading it cannot authorize a model call.

mod projection;
mod schema;

pub use projection::{Projection, render_projections};
pub use schema::{
    ActualBillingKind, Catalog, CatalogError, CommercialChannel, Confidence, DatePrecision,
    MappingQuality, ModelIdentityKind, PriceResolution, Provider, RateDimension,
    ReferenceResolution, ResolvedDateWindow, ResolvedRate, ResolvedRateSet,
};

/// Canonical embedded OpenAI/Anthropic reference catalog JSON.
pub const BUNDLED_CATALOG_JSON: &str = include_str!("../data/reference-catalog.json");

/// Parses and validates the catalog shipped with this crate.
pub fn bundled_catalog() -> Result<Catalog, CatalogError> {
    Catalog::from_json(BUNDLED_CATALOG_JSON)
}
