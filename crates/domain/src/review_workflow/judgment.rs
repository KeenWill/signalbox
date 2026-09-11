//! Categorical review judgments and independent verdict confidence.

use super::{ReviewConfidence, ReviewText};

/// A category under which a review finding qualifies for acceptance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewBarCategory {
    /// An assertion contradicted by evidence.
    FalseStatement,
    /// A reference that does not resolve.
    BrokenReference,
    /// Conflicting committed statements or behavior.
    Contradiction,
    /// An undecided item asserted as committed.
    UndecidedAsCommitted,
    /// A failing validation gate on the reviewed head.
    FailingGate,
    /// A concrete defect in the change's own behavior.
    OwnBehaviorDefect,
}

/// Why a candidate does not qualify under the acceptance bar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewDeclineClass {
    /// Hardening without a concrete failing scenario.
    HypotheticalHardening,
    /// Restoring an inventory already owned by code.
    InventoryRestoration,
    /// Work outside the requested change.
    ScopeExpansion,
    /// A pre-existing defect outside the change.
    PreExistingOutOfScope,
    /// A demand for design documentation.
    DesignDocumentDemand,
    /// A style or prose preference.
    StyleOrProse,
    /// A specification sentence does not justify changing correct behavior.
    SpecSentenceWrong,
    /// Another candidate covers the same defect.
    Duplicate,
    /// Another reason outside the acceptance bar.
    Other,
}

/// Independent judge confidence from one through five.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ReviewJudgeConfidence(u8);

impl ReviewJudgeConfidence {
    /// Checks the closed one-through-five confidence range.
    pub const fn try_new(value: u8) -> Option<Self> {
        if value >= 1 && value <= 5 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Returns the judge's confidence bucket.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Expresses the five-point score on the frozen policy's basis-point scale.
    pub const fn policy_confidence(self) -> ReviewConfidence {
        ReviewConfidence(self.0 as u16 * 2_000)
    }
}

/// The mutually exclusive acceptance category or decline class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewBarVerdict {
    /// The finding qualifies under this acceptance category.
    Accept(ReviewBarCategory),
    /// No acceptance category applies.
    None(ReviewDeclineClass),
}

/// One categorical judgment with its own confidence and explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewJudgment {
    verdict: ReviewBarVerdict,
    confidence: ReviewJudgeConfidence,
    reason: ReviewText,
}

impl ReviewJudgment {
    /// Binds a categorical verdict to independent confidence and its reason.
    pub const fn new(
        verdict: ReviewBarVerdict,
        confidence: ReviewJudgeConfidence,
        reason: ReviewText,
    ) -> Self {
        Self {
            verdict,
            confidence,
            reason,
        }
    }

    /// Returns the category or decline class.
    pub const fn verdict(&self) -> ReviewBarVerdict {
        self.verdict
    }

    /// Returns confidence in this verdict.
    pub const fn confidence(&self) -> ReviewJudgeConfidence {
        self.confidence
    }

    /// Borrows the judgment reason.
    pub const fn reason(&self) -> &ReviewText {
        &self.reason
    }
}

impl ReviewBarCategory {
    /// Returns the categorical judgment key.
    pub const fn key(self) -> &'static str {
        match self {
            Self::FalseStatement => "false-statement",
            Self::BrokenReference => "broken-reference",
            Self::Contradiction => "contradiction",
            Self::UndecidedAsCommitted => "undecided-as-committed",
            Self::FailingGate => "failing-gate",
            Self::OwnBehaviorDefect => "own-behavior-defect",
        }
    }
    /// Resolves a closed categorical judgment key.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "false-statement" => Some(Self::FalseStatement),
            "broken-reference" => Some(Self::BrokenReference),
            "contradiction" => Some(Self::Contradiction),
            "undecided-as-committed" => Some(Self::UndecidedAsCommitted),
            "failing-gate" => Some(Self::FailingGate),
            "own-behavior-defect" => Some(Self::OwnBehaviorDefect),
            _ => None,
        }
    }
}

impl ReviewDeclineClass {
    /// Returns the categorical judgment key.
    pub const fn key(self) -> &'static str {
        match self {
            Self::HypotheticalHardening => "hypothetical-hardening",
            Self::InventoryRestoration => "inventory-restoration",
            Self::ScopeExpansion => "scope-expansion",
            Self::PreExistingOutOfScope => "pre-existing-out-of-scope",
            Self::DesignDocumentDemand => "design-document-demand",
            Self::StyleOrProse => "style-or-prose",
            Self::SpecSentenceWrong => "spec-sentence-wrong",
            Self::Duplicate => "duplicate",
            Self::Other => "other",
        }
    }
    /// Resolves a closed categorical judgment key.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "hypothetical-hardening" => Some(Self::HypotheticalHardening),
            "inventory-restoration" => Some(Self::InventoryRestoration),
            "scope-expansion" => Some(Self::ScopeExpansion),
            "pre-existing-out-of-scope" => Some(Self::PreExistingOutOfScope),
            "design-document-demand" => Some(Self::DesignDocumentDemand),
            "style-or-prose" => Some(Self::StyleOrProse),
            "spec-sentence-wrong" => Some(Self::SpecSentenceWrong),
            "duplicate" => Some(Self::Duplicate),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}
