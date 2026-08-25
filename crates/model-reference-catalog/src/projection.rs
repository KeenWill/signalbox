use crate::schema::Catalog;

/// One deterministic, human-inspectable projection of the canonical catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Projection {
    /// Repository-relative filename below this crate's `projections/` directory.
    pub filename: &'static str,
    /// Complete UTF-8 Markdown contents.
    pub contents: String,
}

/// Renders every checked-in inspection projection in filename order.
pub fn render_projections(catalog: &Catalog) -> Vec<Projection> {
    vec![
        Projection {
            filename: "consumer-equivalence.md",
            contents: catalog.render_consumer_equivalence(),
        },
        Projection {
            filename: "historical-pricing.md",
            contents: catalog.render_historical_pricing(),
        },
        Projection {
            filename: "models.md",
            contents: catalog.render_models(),
        },
        Projection {
            filename: "research-gaps.md",
            contents: catalog.render_research_gaps(),
        },
        Projection {
            filename: "sources.md",
            contents: catalog.render_sources(),
        },
    ]
}
