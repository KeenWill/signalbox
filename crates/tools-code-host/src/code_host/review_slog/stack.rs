//! Parent and immediate-child stack ancestry evidence.

use serde_json::{Value, json};

use crate::code_host::{
    CodeHostNumericBounds,
    arguments::{valid_cursor, valid_revision},
    result::valid_required_text,
};

/// One immediate child in a change-request stack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildStackState {
    number: u32,
    head_ref: String,
    head_revision: String,
    base_commits_not_in_head: u64,
    main_commits_not_in_base: u64,
}

impl ChildStackState {
    /// Validates one child comparison projection.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        number: u32,
        head_ref: String,
        head_revision: String,
        base_commits_not_in_head: u64,
        main_commits_not_in_base: u64,
    ) -> Option<Self> {
        (number > 0 && valid_required_text(bounds, &head_ref) && valid_revision(&head_revision))
            .then_some(Self {
                number,
                head_ref,
                head_revision,
                base_commits_not_in_head,
                main_commits_not_in_base,
            })
    }

    fn into_value(self) -> Value {
        json!({
            "base_commits_not_in_head": self.base_commits_not_in_head,
            "base_needs_merge_forward": self.base_commits_not_in_head > 0,
            "head_ref": self.head_ref,
            "head_revision": self.head_revision,
            "main_commits_not_in_base": self.main_commits_not_in_base,
            "main_missing_from_base_chain": self.main_commits_not_in_base > 0,
            "number": self.number,
        })
    }
}

/// Typed result of `change_request_stack_state`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackStateResult {
    number: u32,
    base_ref: String,
    base_revision: String,
    head_ref: String,
    head_revision: String,
    default_ref: String,
    default_revision: String,
    base_commits_not_in_head: u64,
    main_commits_not_in_base: u64,
    children: Vec<ChildStackState>,
    children_truncated: bool,
    children_next_cursor: Option<String>,
}

/// Complete checked fields for stack-state construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackStateFields {
    /// Change-request number.
    pub number: u32,
    /// Exact immediate base branch.
    pub base_ref: String,
    /// Exact immediate base revision.
    pub base_revision: String,
    /// Exact head branch.
    pub head_ref: String,
    /// Exact head revision.
    pub head_revision: String,
    /// Repository default branch, used as the main-chain authority.
    pub default_ref: String,
    /// Exact default-branch revision.
    pub default_revision: String,
    /// Base commits absent from the change-request head.
    pub base_commits_not_in_head: u64,
    /// Default-branch commits absent from the immediate base.
    pub main_commits_not_in_base: u64,
    /// Bounded immediate-child page.
    pub children: Vec<ChildStackState>,
    /// Whether more immediate children exist.
    pub children_truncated: bool,
    /// Opaque next child-page cursor.
    pub children_next_cursor: Option<String>,
}

impl StackStateResult {
    /// Validates one bounded stack-state result.
    pub fn try_new(bounds: CodeHostNumericBounds, fields: StackStateFields) -> Option<Self> {
        let cursor_valid = fields
            .children_next_cursor
            .as_deref()
            .is_none_or(valid_cursor);
        let valid = fields.number > 0
            && valid_required_text(bounds, &fields.base_ref)
            && valid_revision(&fields.base_revision)
            && valid_required_text(bounds, &fields.head_ref)
            && valid_revision(&fields.head_revision)
            && valid_required_text(bounds, &fields.default_ref)
            && valid_revision(&fields.default_revision)
            && bounds.permits_result_items(fields.children.len())
            && cursor_valid
            && fields.children_truncated == fields.children_next_cursor.is_some();
        valid.then_some(Self {
            number: fields.number,
            base_ref: fields.base_ref,
            base_revision: fields.base_revision,
            head_ref: fields.head_ref,
            head_revision: fields.head_revision,
            default_ref: fields.default_ref,
            default_revision: fields.default_revision,
            base_commits_not_in_head: fields.base_commits_not_in_head,
            main_commits_not_in_base: fields.main_commits_not_in_base,
            children: fields.children,
            children_truncated: fields.children_truncated,
            children_next_cursor: fields.children_next_cursor,
        })
    }

    pub(super) fn into_value(self) -> Value {
        json!({
            "base_commits_not_in_head": self.base_commits_not_in_head,
            "base_needs_merge_forward": self.base_commits_not_in_head > 0,
            "base_ref": self.base_ref,
            "base_revision": self.base_revision,
            "children": self.children.into_iter().map(ChildStackState::into_value).collect::<Vec<_>>(),
            "children_next_cursor": self.children_next_cursor,
            "children_truncated": self.children_truncated,
            "default_ref": self.default_ref,
            "default_revision": self.default_revision,
            "head_ref": self.head_ref,
            "head_revision": self.head_revision,
            "main_commits_not_in_base": self.main_commits_not_in_base,
            "main_missing_from_base_chain": self.main_commits_not_in_base > 0,
            "number": self.number,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stack continuations use the same opaque cursor admission as arguments.
    #[test]
    fn opaque_child_cursor_is_admitted() {
        const BASE_REVISION: &str = "1111111111111111111111111111111111111111";
        const HEAD_REVISION: &str = "2222222222222222222222222222222222222222";
        let result = StackStateResult::try_new(
            crate::code_host::test_numeric_bounds(),
            StackStateFields {
                number: 17,
                base_ref: String::from("main"),
                base_revision: String::from(BASE_REVISION),
                head_ref: String::from("feature"),
                head_revision: String::from(HEAD_REVISION),
                default_ref: String::from("main"),
                default_revision: String::from(BASE_REVISION),
                base_commits_not_in_head: 0,
                main_commits_not_in_base: 0,
                children: Vec::new(),
                children_truncated: true,
                children_next_cursor: Some(String::from("opaque-child-page")),
            },
        );

        assert!(result.is_some());
    }
}
