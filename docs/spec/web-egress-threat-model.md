# Web egress threat model

This page specifies the implemented security boundary for web-tool output. It
covers provider response parsing and evidence construction in
`crates/tools-web`, together with the defense-in-depth approval boundary for web
egress.

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

## Approval defense in depth

`web_search` and `web_fetch` are daemon-composed egress tools with
`ExternalEffect` declarations. Both declarations default to `Confirm`, and the
shipped configuration resolves both exact tool names to the `Human` approval
posture. The resulting precedence over the session blanket and the durable
approval flow are owned by [Approval policy and decision sources](tool-loop.md).
An operator must therefore make an explicit policy choice before either tool can
execute automatically.

The approval boundary remains necessary even when an egress transport has an
exact destination policy. A model can combine content read from a workspace with
a `web_fetch` URL, or use a code-host read as authority for a subsequent search,
without either source authorizing disclosure or delegation. Origin admission
constrains the recipient; it does not establish the user's intent to send data
or let one external system direct another. Why: conservative declarations keep
deliberate operator policy and the ordinary approval flow as the only ways to
widen egress authority.

## Review rule

A finding that demonstrates a violation of the structural output boundary or a
named bound is an implementation defect. A finding solely about a transformed,
encoded, or split secret that exact redaction cannot recognize, or about
provider-content semantics, is an accepted residual and does not require a code
change.

## Open edges

None.
