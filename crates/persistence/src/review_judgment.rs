//! Private categorical judgment storage representation.

use serde::{Deserialize, Serialize};
use signalbox_domain::{
    ReviewBarCategory, ReviewBarVerdict, ReviewDeclineClass, ReviewJudgeConfidence, ReviewJudgment,
    ReviewText,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredReviewJudgment {
    bar_category: String,
    decline_class: Option<String>,
    confidence: u8,
    reason: String,
}

impl StoredReviewJudgment {
    pub(crate) fn encode(judgment: &ReviewJudgment) -> Self {
        let (bar_category, decline_class) = match judgment.verdict() {
            ReviewBarVerdict::Accept(category) => (category.key(), None),
            ReviewBarVerdict::None(class) => ("none", Some(class.key().to_owned())),
        };
        Self {
            bar_category: bar_category.to_owned(),
            decline_class,
            confidence: judgment.confidence().get(),
            reason: judgment.reason().as_str().to_owned(),
        }
    }

    pub(crate) fn decode(self) -> Option<ReviewJudgment> {
        let verdict = match (self.bar_category.as_str(), self.decline_class.as_deref()) {
            ("none", Some(class)) => ReviewBarVerdict::None(ReviewDeclineClass::from_key(class)?),
            (category, None) => ReviewBarVerdict::Accept(ReviewBarCategory::from_key(category)?),
            _ => return None,
        };
        Some(ReviewJudgment::new(
            verdict,
            ReviewJudgeConfidence::try_new(self.confidence)?,
            ReviewText::try_new(self.reason).ok()?,
        ))
    }
}
