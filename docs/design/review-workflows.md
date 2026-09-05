# Review workflows design

This design is not built; it extends
[review workflows](../spec/review-workflows.md).

## Goal

An orchestration attempt runs from start to a complete outcome without a client
feeding each stage. Concrete provider, model, and workspace adapters implement
the application's runner ports; a model runtime returns findings through one
structured tool call; a blocked repair resumes after its findings are
reconciled; a completed publication imports the external context it produced.

## Design

Three adapters implement the runner ports the application exposes for
external-context import, session-backed passes, repair, and reserved
publication: a provider adapter for the code host, a model adapter for the
review model, and a workspace adapter for checkouts. The workspace adapter
prepares a read-only checkout for review and judgment passes, or an explicitly
writable checkout for repair passes, always at the exact target head, with the
base revision available for comparison when the target carries one. The provider
adapter constructs an external object identifier as an opaque canonical
provider-wide key: it qualifies a repository-scoped host identifier with the
canonical repository key before it constructs the attachment.

The model adapter exposes one tool, `submit_review_findings`, disables parallel
tool use, and decodes the call's arguments independently of the provider.
Exactly one call to that tool is the pass's success condition; the adapter does
not force the call, because the Anthropic runtime carries a named tool demand as
an instruction only ([runtime substrate](../spec/runtime-substrate.md)). The
argument is one object with one required member, `findings`, an array of at most
32 finding objects; no object admits additional properties. Every finding object
requires all eleven members: `file_path`, a string; `line_start` and `line_end`,
either both null or both integers from 1 through 4294967295; `diff_side`, null,
`left`, or `right`; `title` and `body`, strings; `severity`, one of `info`,
`low`, `medium`, `high`, or `critical`; `is_real_confidence` and
`severity_label_confidence`, integers from 0 through 10000; `category`, a
string; and `recommended_fix`, a string or null. After decode, ordinary finding
construction enforces that `line_end` is not below `line_start`, the byte
bounds, the nonempty and U+0000 rules, target comparison evidence, and every
typed vocabulary.

The adapter fails the pass on no structured value, several values, malformed
JSON, schema mismatch, a domain-invalid item, more than 32 items, or a failed
inventory admission. It assigns stable finding identities and admits the entire
canonical identity-ordered inventory atomically through the complete-findings
path; no proposal survives as untyped text or as a partial inventory, and
free-form assistant text stays transcript evidence.

A new application-store operation replaces the sealed incomplete repair outcome
once every blocked finding has taken a transition the finding rules admit after
a repair block: fixed, superseded, or stale. No blocked finding returns to the
publication set; publication becomes eligible against the accepted findings the
repair neither fixed nor blocked.

After publication completes, a continuation runs one external-context-import
pass per posted link. The work names the attempt and the link, and the store
keeps one continuation slot per attempt and link, so a resumed attempt tells the
passes apart. A newly posted link carries no observation, so the pass appends
observation one. A no-change result applies only after that observation exists;
a resumed pass that finds it recorded binds one. The continuation never infers
external state from the publication result.

## Compatibility constraints

Adapter success keeps returning typed evidence that names the exact target,
policy, run, pass, session, and template inputs, so a concrete adapter fits the
ports without a new evidence shape.

A finding proposal stays typed content, and no path from assistant text to a
finding is added.

The sealed incomplete repair outcome stays a typed durable record that a later
operation can replace; nothing derives publication eligibility from a blocked
attempt in the meantime.

The no-change pass result stays in the result vocabulary, because a resumed
continuation pass depends on it.

The external-link attachment stores its identifier as an opaque key and does not
interpret it.

## Acceptance criteria

A start selection against a resolved review library runs import, fan-out,
judgment, repair, and publication to a complete outcome with no client
submission.

The review model's only route to a finding is one `submit_review_findings` call;
a pass with no call, several calls, or an invalid payload fails and admits no
finding.

Every adapter checkout is at the exact target head, with the base revision
available for comparison when the target carries one; only a repair checkout is
writable.

An attachment identifier for a repository-scoped host object is the qualified
canonical key.

A blocked repair, once each blocked finding is fixed, superseded, or stale,
reaches publication without a new attempt, and no blocked finding is posted.

A completed publication is followed by one import pass per posted link, recorded
under the attempt and link, and each link carries observation one afterward.
