//! Review reference for `docs/spec/review-workflows.md`.

use super::{ReviewFindingId, ReviewPassId, ReviewRunId, ReviewTargetId};

/// A target-bound review-run reference.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewRunRef {
    target: ReviewTargetId,
    run: ReviewRunId,
}

impl ReviewRunRef {
    /// Binds one run identity to its target.
    pub const fn new(target: ReviewTargetId, run: ReviewRunId) -> Self {
        Self { target, run }
    }

    /// Returns the target identity.
    pub const fn target(self) -> ReviewTargetId {
        self.target
    }

    /// Returns the run identity.
    pub const fn run(self) -> ReviewRunId {
        self.run
    }
}

/// A run-bound review-pass reference carrying complete ancestry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewPassRef {
    run: ReviewRunRef,
    pass: ReviewPassId,
}

impl ReviewPassRef {
    /// Binds one pass identity to its exact run.
    pub const fn new(run: ReviewRunRef, pass: ReviewPassId) -> Self {
        Self { run, pass }
    }

    /// Returns the run reference.
    pub const fn run(self) -> ReviewRunRef {
        self.run
    }

    /// Returns the pass identity.
    pub const fn pass(self) -> ReviewPassId {
        self.pass
    }

    /// Returns the target identity.
    pub const fn target(self) -> ReviewTargetId {
        self.run.target()
    }
}

/// A finding reference carrying complete target/run/producing-pass ancestry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewFindingRef {
    pass: ReviewPassRef,
    finding: ReviewFindingId,
}

impl ReviewFindingRef {
    /// Binds one finding identity to its exact producing pass.
    pub const fn new(pass: ReviewPassRef, finding: ReviewFindingId) -> Self {
        Self { pass, finding }
    }

    /// Returns the producing-pass reference.
    pub const fn pass(self) -> ReviewPassRef {
        self.pass
    }

    /// Returns the run reference.
    pub const fn run(self) -> ReviewRunRef {
        self.pass.run()
    }

    /// Returns the finding identity.
    pub const fn finding(self) -> ReviewFindingId {
        self.finding
    }

    /// Returns the target identity.
    pub const fn target(self) -> ReviewTargetId {
        self.pass.target()
    }
}
