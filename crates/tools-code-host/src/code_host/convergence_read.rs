//! Bounded tool projection of the shared convergence verdict.
use serde_json::{Value, json};
use signalbox_convergence::{Evaluation, Verdict};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergenceReadResult {
    head_revision: String,
    verdict: Verdict,
}

impl ConvergenceReadResult {
    /// Projects a crate verdict within the code-host result bounds.
    pub fn try_new(bounds: super::CodeHostNumericBounds, evaluation: Evaluation) -> Option<Self> {
        let result = Self { head_revision:evaluation.facts.head_oid,verdict:evaluation.verdict };
        let encoded = serde_json::to_vec(&result.clone().into_value()).ok()?;
        (super::arguments::valid_revision(&result.head_revision)
            && result.verdict.reasons().len() <= 100
            && bounds.permits_result_items(result.verdict.reasons().len())
            && encoded.len() <= super::result::MAX_ENCODED_RESULT_BYTES).then_some(result)
    }

    pub(crate) fn into_value(self) -> Value {
        json!({"head_revision":self.head_revision,"convergence":self.verdict})
    }
}
