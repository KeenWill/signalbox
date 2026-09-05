# Web egress threat model

The web egress boundary reduces provider bytes to bounded evidence and keeps
every outbound web request under operator policy and approval.

## Map

The daemon composes two web tools, `web_search` and `web_fetch`, in
`crates/tools-web`. Each invocation sends at most one request and returns the
outcome as evidence for the model.

The `web_search` output boundary is structural: the provider response is parsed
into typed components and the evidence is built from those components, each
admitted within a named constant beside the type that enforces it. After
construction, a `CredentialScrubber` redacts the search credential's exact value
and encoded variants from that output. A `web_fetch` response is not parsed into
components: its evidence carries the response body as decoded text under a named
byte bound, with the status, the content type, and a truncation flag.

The transport floor in `crates/egress-transport` fixes how a `web_fetch`
connection is made. Origin admission, stated in
[configuration-and-credentials.md](configuration-and-credentials.md), fixes
which origins `web_fetch` may reach; `web_search` reaches one fixed provider.
The approval flow, both tools' declarations, and their shipped human posture are
stated in [tool-loop.md](tool-loop.md). Transport and admission constrain where
bytes go, approval constrains whether they go, and structure constrains what
comes back.

## Decisions

Structural construction is the primary control on `web_search` output, and the
credential scrubber is defense in depth after it. Why: parsing treats provider
bytes as input to a bounded parser rather than as a second rendering channel,
and exact-value redaction may miss a credential that provider content
transforms, encodes, or splits; those forms are accepted residual risk.

The trustworthiness, relevance, and safety of provider content are outside this
model, which constrains how content is represented, not what it means.

Origin admission constrains the recipient only; it establishes no user intent to
send data and lets no external system direct another, so approval stays
necessary even when a transport has an exact destination policy. Why: a model
can put workspace content into a fetch URL, or take a code-host read as
authority for a later search, and neither source authorizes that disclosure or
delegation.

Both tools' declarations are conservative, so deliberate operator policy and the
ordinary approval flow are the only ways to widen egress authority.

A demonstrated violation of the structural output boundary or a named bound is
an implementation defect. A finding only about a residual named above, or about
provider-content semantics, is accepted and needs no code change.

## Contracts

A `web_fetch` destination resolves to between one and 32 public addresses, and
the transport pins those addresses into the client so connection setup cannot
substitute a later DNS answer. One dispatch performs at most one credential-free
request; proxies, redirects, retries, and idle connection reuse are disabled,
TLS runs on rustls with a 1.2 floor, and one 15-second timeout bounds resolution
and the exchange. `crates/egress-transport` builds this client.

## Not built

None.
