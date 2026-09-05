//! Structured review-thread inventory and deterministic classifications.

use serde_json::{Value, json};

use crate::code_host::{
    CodeHostNumericBounds,
    arguments::{valid_cursor, valid_opaque_id, valid_revision},
    result::{valid_path, valid_required_text, valid_text},
    review_slog::ESCALATION_MARKER,
};

/// Whether the code host identifies an actor as a repository participant who
/// can speak for the review protocol.
pub(crate) fn authorized_association(association: &str) -> bool {
    matches!(association, "OWNER" | "MEMBER" | "COLLABORATOR")
}

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

/// Deterministic classification of a thread's recorded disposition.
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
    /// Deterministic recorded disposition class.
    pub disposition: ReviewDispositionClass,
}

impl ReviewThreadInventoryItem {
    /// Validates one inventory item.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        fields: ReviewThreadInventoryFields,
    ) -> Option<Self> {
        (valid_opaque_id(&fields.id)
            && valid_path(&fields.path)
            && fields
                .author
                .as_deref()
                .is_none_or(|value| valid_required_text(bounds, value))
            && valid_text(bounds, &fields.finding_title))
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

    /// Returns the recorded disposition classification.
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
        bounds: CodeHostNumericBounds,
        head_revision: String,
        threads: Vec<ReviewThreadInventoryItem>,
        truncated: bool,
        next_cursor: Option<String>,
    ) -> Option<Self> {
        (valid_revision(&head_revision)
            && bounds.permits_result_items(threads.len())
            && next_cursor.as_deref().is_none_or(valid_cursor)
            && truncated == next_cursor.is_some())
        .then_some(Self {
            threads,
            head_revision,
            truncated,
            next_cursor,
        })
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

/// Classifies a thread's ordered replies under the recorded conventions.
///
/// An escalation is current only when the exact last reply carries its marker.
/// Otherwise the latest recognized fix or decline survives later
/// non-disposition replies.
pub(crate) fn disposition_class(reply_evidence: &[(&str, bool)]) -> ReviewDispositionClass {
    let Some((last_body, last_authorized)) = reply_evidence.last() else {
        return ReviewDispositionClass::Undispositioned;
    };
    if *last_authorized && last_body.trim() == ESCALATION_MARKER {
        return ReviewDispositionClass::EscalationMarker;
    }

    reply_evidence
        .iter()
        .rev()
        .filter(|(_, authorized)| *authorized)
        .find_map(|(body, _)| disposition_reply_class(body))
        .unwrap_or(ReviewDispositionClass::Undispositioned)
}

fn disposition_reply_class(body: &str) -> Option<ReviewDispositionClass> {
    let body = body.trim_start();
    if body
        .strip_prefix("Declined:")
        .is_some_and(|reason| !reason.trim().is_empty())
    {
        return Some(ReviewDispositionClass::Declined);
    }
    let fixing_commits = body
        .strip_prefix("Fixed in commit ")
        .or_else(|| body.strip_prefix("Fixed in commits "));
    if fixing_commits.is_some_and(contains_commit_token) {
        return Some(ReviewDispositionClass::FixNamed);
    }
    None
}

fn contains_commit_token(body: &str) -> bool {
    let body = body.strip_prefix('`').unwrap_or(body);
    let revision: String = body
        .chars()
        .take_while(|character| character.is_ascii_hexdigit())
        .take(40)
        .collect();
    if !(7..=40).contains(&revision.len()) {
        return false;
    }
    body.get(revision.len()..).is_some_and(|trailing| {
        trailing
            .strip_prefix('`')
            .unwrap_or(trailing)
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disposition-shaped reply from an unaffiliated actor is not protocol
    /// evidence.
    #[test]
    fn unauthorized_disposition_remains_undispositioned() {
        assert_eq!(
            disposition_class(&[("Fixed in commit `0123456789abcdef`", false)]),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// A fixing reply must name a commit before it can leave the
    /// undispositioned class.
    #[test]
    fn fixing_commit_is_classified_as_fix_named() {
        assert_eq!(
            disposition_class(&[("Fixed in commit `0123456789abcdef`", true)]),
            ReviewDispositionClass::FixNamed
        );
    }

    /// A hexadecimal fixture later in narrative text cannot impersonate the
    /// fixing revision required immediately after the disposition prefix.
    #[test]
    fn narrative_hex_after_fix_prefix_remains_undispositioned() {
        assert_eq!(
            disposition_class(&[(
                "Fixed in commit not yet; fixture 0123456 demonstrates the issue.",
                true
            )]),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// Additional whitespace after the recorded prefix cannot defer the
    /// required immediate fixing revision.
    #[test]
    fn extra_space_before_fixing_revision_remains_undispositioned() {
        assert_eq!(
            disposition_class(&[("Fixed in commit  0123456", true)]),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// An explicit declined reply is retained as its own disposition class.
    #[test]
    fn declined_reply_is_classified_as_declined() {
        assert_eq!(
            disposition_class(&[("Declined: the cited contract requires this shape.", true)]),
            ReviewDispositionClass::Declined
        );
    }

    /// The hard-stop marker wins over other words in the reply.
    #[test]
    fn escalation_marker_is_classified_exactly() {
        assert_eq!(
            disposition_class(&[("Escalated without disposition", true)]),
            ReviewDispositionClass::EscalationMarker
        );
    }

    /// A narrative mention of the marker is not the named hard-stop
    /// disposition.
    #[test]
    fn narrative_escalation_mention_remains_undispositioned() {
        assert_eq!(
            disposition_class(&[(
                "This should not be marked Escalated without disposition.",
                true
            )]),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// A narrative reply without the recorded disposition evidence remains a
    /// machine-visible work item.
    #[test]
    fn narrative_reply_remains_undispositioned() {
        assert_eq!(
            disposition_class(&[("Thanks, I will inspect this.", true)]),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// Mentioning a decline without the recorded prefix and reason is not a
    /// disposition.
    #[test]
    fn narrative_decline_mention_remains_undispositioned() {
        assert_eq!(
            disposition_class(&[("This should not be declined.", true)]),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// A decline prefix without its required reason is not a disposition.
    #[test]
    fn reasonless_decline_remains_undispositioned() {
        assert_eq!(
            disposition_class(&[("Declined:   ", true)]),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// Narrative use of a word containing `fix` and a hexadecimal token does
    /// not impersonate the recorded fixing-commit reply shape.
    #[test]
    fn narrative_fix_token_remains_undispositioned() {
        assert_eq!(
            disposition_class(&[(
                "The fixture 0123456 demonstrates the issue but names no fix.",
                true
            )]),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// A finding cannot classify itself as fixed when no reply exists.
    #[test]
    fn finding_body_without_reply_remains_undispositioned() {
        assert_eq!(
            disposition_class(&[]),
            ReviewDispositionClass::Undispositioned
        );
    }

    /// A later informational reply does not erase a fixing disposition.
    #[test]
    fn informational_reply_preserves_latest_recognized_disposition() {
        assert_eq!(
            disposition_class(&[
                ("Fixed in commit `0123456789abcdef`", true),
                ("Thank you; that answers my question.", true),
            ]),
            ReviewDispositionClass::FixNamed
        );
    }

    /// An escalation marker is current only while it remains the last reply.
    #[test]
    fn informational_reply_buries_no_escalation_marker() {
        assert_eq!(
            disposition_class(&[
                ("Declined: the cited contract requires this shape.", true),
                ("Escalated without disposition", true),
                ("The owner answered the pending question.", true),
            ]),
            ReviewDispositionClass::Declined
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
