//! Structured review-thread inventory and deterministic classifications.

use serde_json::{Value, json};

use crate::code_host::{
    arguments::{valid_cursor, valid_opaque_id, valid_revision},
    result::{MAX_RESULT_ITEMS, valid_path, valid_required_text, valid_text},
    review_slog::ESCALATION_MARKER,
};

/// Classification of the first review-thread author.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewAuthorClass {
    /// GitHub reports a bot actor.
    Bot,
    /// GitHub reports a human user actor.
    Human,
    /// The actor is absent or another additive GraphQL actor type.
    Unknown,
}

impl ReviewAuthorClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::Human => "human",
            Self::Unknown => "unknown",
        }
    }
}

/// Deterministic classification of a thread's last comment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewDispositionClass {
    /// The reply names a fixing commit.
    FixNamed,
    /// The reply explicitly declines the finding.
    Declined,
    /// The reply carries the exact escalation marker.
    EscalationMarker,
    /// No recognized disposition is present.
    Undispositioned,
}

impl ReviewDispositionClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FixNamed => "fix_named",
            Self::Declined => "declined",
            Self::EscalationMarker => "escalation_marker",
            Self::Undispositioned => "undispositioned",
        }
    }
}

/// One bounded structured review-thread inventory item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewThreadInventoryItem {
    id: String,
    path: String,
    line: Option<u64>,
    resolved: bool,
    outdated: bool,
    author: Option<String>,
    author_class: ReviewAuthorClass,
    finding_title: String,
    disposition: ReviewDispositionClass,
}

/// Complete checked fields for one inventory item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewThreadInventoryFields {
    /// Opaque review-thread identity.
    pub id: String,
    /// Exact repository-relative path.
    pub path: String,
    /// Optional current line.
    pub line: Option<u64>,
    /// Code-host resolution posture.
    pub resolved: bool,
    /// Code-host outdated posture.
    pub outdated: bool,
    /// Optional exact first-comment author login.
    pub author: Option<String>,
    /// Deterministic author class.
    pub author_class: ReviewAuthorClass,
    /// Finding title derived from the first comment.
    pub finding_title: String,
    /// Deterministic last-comment disposition class.
    pub disposition: ReviewDispositionClass,
}

impl ReviewThreadInventoryItem {
    /// Validates one inventory item.
    pub fn try_new(fields: ReviewThreadInventoryFields) -> Option<Self> {
        (valid_opaque_id(&fields.id)
            && valid_path(&fields.path)
            && fields.author.as_deref().is_none_or(valid_required_text)
            && valid_text(&fields.finding_title))
        .then_some(Self {
            id: fields.id,
            path: fields.path,
            line: fields.line,
            resolved: fields.resolved,
            outdated: fields.outdated,
            author: fields.author,
            author_class: fields.author_class,
            finding_title: fields.finding_title,
            disposition: fields.disposition,
        })
    }

    /// Borrows the opaque thread identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the last-comment disposition classification.
    pub const fn disposition(&self) -> ReviewDispositionClass {
        self.disposition
    }

    fn into_value(self) -> Value {
        json!({
            "author": self.author,
            "author_class": self.author_class.as_str(),
            "disposition": self.disposition.as_str(),
            "finding_title": self.finding_title,
            "id": self.id,
            "line": self.line,
            "outdated": self.outdated,
            "path": self.path,
            "resolved": self.resolved,
        })
    }
}

/// Typed result of `change_request_thread_inventory`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadInventoryResult {
    head_revision: String,
    threads: Vec<ReviewThreadInventoryItem>,
    truncated: bool,
    next_cursor: Option<String>,
}

impl ThreadInventoryResult {
    /// Validates one bounded inventory page and its honest continuation.
    pub fn try_new(
        head_revision: String,
        threads: Vec<ReviewThreadInventoryItem>,
        truncated: bool,
        next_cursor: Option<String>,
    ) -> Option<Self> {
        (valid_revision(&head_revision)
            && threads.len() <= MAX_RESULT_ITEMS
            && next_cursor.as_deref().is_none_or(valid_cursor)
            && truncated == next_cursor.is_some())
        .then_some(Self {
            threads,
            head_revision,
            truncated,
            next_cursor,
        })
    }

    pub(super) const fn truncated(&self) -> bool {
        self.truncated
    }

    pub(super) fn head_revision(&self) -> &str {
        &self.head_revision
    }

    pub(super) fn undispositioned_ids(&self) -> Vec<String> {
        self.threads
            .iter()
            .filter(|thread| thread.disposition == ReviewDispositionClass::Undispositioned)
            .map(|thread| thread.id.clone())
            .collect()
    }

    pub(super) fn unresolved_ids(&self) -> Vec<String> {
        self.threads
            .iter()
            .filter(|thread| !thread.resolved)
            .map(|thread| thread.id.clone())
            .collect()
    }

    pub(super) fn into_value(self) -> Value {
        json!({
            "next_cursor": self.next_cursor,
            "head_revision": self.head_revision,
            "threads": self.threads.into_iter().map(ReviewThreadInventoryItem::into_value).collect::<Vec<_>>(),
            "truncated": self.truncated,
        })
    }
}

/// Extracts the first nonempty finding-title line using the existing bot-badge
/// convention and the prior audit's 100-character display bound.
pub(crate) fn finding_title(body: &str) -> String {
    let line = body.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(line) = line else {
        return String::from("(empty)");
    };
    let without_emphasis = line.replace("**", "");
    let without_badge = without_emphasis
        .rsplit_once("</sub></sub>")
        .map_or(without_emphasis.as_str(), |(_, suffix)| suffix)
        .trim();
    without_badge.chars().take(100).collect()
}

/// Classifies a first-comment GraphQL actor type without rewriting its login.
pub(crate) fn author_class(actor_type: Option<&str>) -> ReviewAuthorClass {
    match actor_type {
        Some("Bot") => ReviewAuthorClass::Bot,
        Some("User") => ReviewAuthorClass::Human,
        Some(_) | None => ReviewAuthorClass::Unknown,
    }
}

/// Classifies an actual reply's exact last-comment text under the recorded
/// conventions.
pub(crate) fn disposition_class(body: &str, reply_exists: bool) -> ReviewDispositionClass {
    if !reply_exists {
        return ReviewDispositionClass::Undispositioned;
    }
    if body.contains(ESCALATION_MARKER) {
        return ReviewDispositionClass::EscalationMarker;
    }
    let lowercase = body.to_ascii_lowercase();
    if lowercase.contains("declined") {
        return ReviewDispositionClass::Declined;
    }
    if lowercase.contains("fix") && contains_commit_token(body) {
        return ReviewDispositionClass::FixNamed;
    }
    ReviewDispositionClass::Undispositioned
}

fn contains_commit_token(body: &str) -> bool {
    body.split(|character: char| !character.is_ascii_hexdigit())
        .any(|token| (7..=40).contains(&token.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixing reply must name a commit before it can leave the
    /// undispositioned class.
    #[test]
    fn fixing_commit_is_classified_as_fix_named() {
        assert_eq!(
            disposition_class("Fixed in commit `0123456789abcdef`", true),
            ReviewDispositionClass::FixNamed
        );
    }

    /// An explicit declined reply is retained as its own disposition class.
    #[test]
    fn declined_reply_is_classified_as_declined() {
        assert_eq!(
            disposition_class("Declined: the cited contract requires this shape.", true),
            ReviewDispositionClass::Declined
        );
    }

    /// The hard-stop marker wins over other words in the reply.
    #[test]
    fn escalation_marker_is_classified_exactly() {
        assert_eq!(
            disposition_class("Escalated without disposition", true),
            ReviewDispositionClass::EscalationMarker
        );
    }

    /// A narrative reply without the recorded disposition evidence remains a
    /// machine-visible work item.
    #[test]
    fn narrative_reply_remains_undispositioned() {
        assert_eq!(
            disposition_class("Thanks, I will inspect this.", true),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// A finding cannot classify itself as fixed when no reply exists.
    #[test]
    fn finding_body_without_reply_remains_undispositioned() {
        assert_eq!(
            disposition_class("Fixed in commit `0123456789abcdef`", false),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// Bot badge markup is removed while the finding's first title line is
    /// retained exactly.
    #[test]
    fn finding_title_removes_only_known_badge_markup() {
        assert_eq!(
            finding_title("badge</sub></sub>**Finding title**\n\nBody"),
            "Finding title"
        );
    }
}
