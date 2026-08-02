# Web egress threat model

This page specifies the implemented security boundary for web-tool output,
verified against PR #365 (`agent/web-search`). It covers provider response
parsing and evidence construction in `crates/tools-basic`; provider selection,
transport behavior, and daemon composition are owned by their implementing
slices.

## Structural output boundary

Provider response bytes are never themselves rendered. Every success or failure
output is constructed from parsed, typed components, and every rendered
component is admitted only within its named byte or item limit.

For search results, URL parsing discards user information before rendering,
retains only query parameters named by the explicit result-query allowlist, and
reconstructs the URL from the retained parsed components. Provider titles and
snippets are entity-escaped before they become evidence. Result count, title,
URL, snippet, provider-response, and failure-detail bounds are named constants
beside the types that enforce them.

Why: structural construction makes provider bytes data for a bounded parser, not
an alternative rendering channel.

## Credential scrubbing

`CredentialScrubber` is defense in depth after structural construction, not the
primary security control. Exact-value redaction cannot guarantee detection of a
credential that provider-controlled content transforms, encodes, or splits.
Those forms are accepted residual risk; structural construction remains the
control that prevents raw provider bytes from becoming output.

The semantic trustworthiness, relevance, and safety of provider-supplied content
are also outside this threat model. The boundary constrains how content is
represented and bounded; it does not endorse what that content says.

## Review rule

A finding that demonstrates a violation of the structural output boundary or a
named bound is an implementation defect. A finding solely about a transformed,
encoded, or split secret that exact redaction cannot recognize, or about
provider-content semantics, is dispositioned as an accepted residual with a
citation to this page and does not require a code change.

## Open edges

None.
