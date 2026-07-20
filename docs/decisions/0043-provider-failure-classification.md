# ADR-0043: Provider failure classification at the send boundary

- Date: 2026-07-20
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0004](0004-turn-and-attempt-lifecycle.md),
  [ADR-0005](0005-model-call-retry-semantics.md),
  [ADR-0017](0017-credential-lifecycle.md), and
  [ADR-0031](0031-direct-fatal-terminalization.md)
- Refines: ADR-0004 and ADR-0005's provider-operation evidence threshold, plus
  ADR-0017's classification of provider-side credential rejection
- Resolves: the
  [provider ambiguity evidence thresholds](../open-questions.md#model-fallback-and-provenance-reserved-adr-0006-adr-0007)
  question that blocks a provider adapter
- Decision questions: exact scripted-adapter classification; the real-adapter
  send boundary; definitive provider responses; live and startup evidence;
  timeout classification without selecting timeout budgets

## Context

ADR-0004 and ADR-0005 already define the consequences of classifying a model
call `KnownFailed` or `Ambiguous`: terminal monotonicity, attempt and turn
precedence, recovery waits, exact reconciliation, and the version-one ban on
automatic retry. They deliberately leave the evidence threshold to provider
contracts. ADR-0017 likewise decides that credential acquisition failure during
send preparation is known failure but leaves provider-side authentication
responses to that open threshold.

Without one shared boundary, adapters can give identical transport evidence
opposite meanings. One might call every socket error retryable known failure;
another might call every error after `InFlight` ambiguous. A scripted adapter
can accidentally encode a third policy in test fixtures. Startup recovery would
then disagree with live handling, contrary to INV-034, and provider SDK
retryability labels could silently become Signalbox lifecycle authority.

The missing decision is classification only. The existing model-call
dispositions and aggregate transitions are sufficient.

## Decision

### Classification is an adapter contract

Every provider adapter exposes a total classification for each outcome it can
report. The hub consumes that classification through the existing ADR-0005
model-call dispositions; it does not reinterpret SDK errors by retryability,
exception type, or generic network-error category.

Provider-target observation remains separate typed evidence under ADR-0005. A
trusted target mismatch keeps ADR-0005's existing precedence over response
material. This record neither changes target-evidence trust nor makes an
otherwise non-authoritative response authoritative.

### Scripted adapters declare the result exactly

Every terminal action in an in-repository scripted provider fixture declares its
exact physical model-call disposition:

```text
Completed | KnownFailed | Refused | Cancelled | Ambiguous
```

The declaration is required. The scripted adapter does not infer it from elapsed
time, missing content, an injected I/O error, or whether a fixture author
considers the failure transient. A script may separately declare target
observations and response material, but those facts do not supply a default
disposition.

This makes scripts deterministic lifecycle fixtures rather than simulations of
an unspecified provider. Fixtures that exercise the real-adapter boundary may
name a pre-send failure, a definitive provider status, or post-send loss, but
the expected classification remains explicit.

### Real adapters classify at full request send

For a real adapter, the decisive transport boundary is **full request send**.
The request is fully sent when the selected transport reports successful
completion of the entire request write, including its end-of-request framing.
This is a local transport fact, not proof that the provider accepted, processed,
or billed the request.

Classification follows this exhaustive rule:

| Evidence                                                                                                                                                                                                       | Physical disposition |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------- |
| The aggregate carries the exact `AppliedInterruptProof` for this predecessor, and evidence proves the request was not accepted and the interrupt prevented all remaining work, including the full request send | `Cancelled`          |
| A failure is observed while the adapter can establish that the full request send did not complete, and the proof-bearing cancellation branch above does not apply                                              | `KnownFailed`        |
| A complete, correlated provider response has a recognized refusal status or payload                                                                                                                            | `Refused`            |
| A complete, correlated provider response definitively confirms provider cancellation and is not recognized as refusal                                                                                          | `Cancelled`          |
| A complete, correlated provider response has a recognized terminal success status and valid completion material and is not recognized as refusal or cancellation                                               | `Completed`          |
| A complete, correlated provider response has an explicit terminal error status and is not recognized as refusal or cancellation                                                                                | `KnownFailed`        |
| The full request was sent, or may have been sent, and no definitive provider response establishes another disposition                                                                                          | `Ambiguous`          |

Pre-send credential lookup, request construction, serialization, connection,
TLS, and incomplete-write failures therefore classify `KnownFailed` when the
adapter has the stated evidence and the proof-bearing cancellation branch does
not apply. Partial response bytes, an unrecognized or truncated response,
connection loss, process loss, and local cancellation after full send are not
definitive provider responses and therefore classify `Ambiguous` unless stronger
evidence establishes a terminal status.

After ADR-0005's trusted-target-mismatch precedence, each real adapter must
define an exhaustive, mutually exclusive mapping of its provider-native terminal
success, refusal, error, and cancellation statuses and payloads. A recognized
refusal status or payload takes precedence over a generic terminal-error
mapping, including when the provider carries the refusal in a generic error
response; only the remaining explicit terminal error responses classify
`KnownFailed`. No native response may match more than one disposition. An
explicit provider error response classified by that mapping is definitive even
though it arrives after full send; observable acceptance, processing, or billing
may be recorded separately and does not change the call disposition. An SDK's
`retryable`, `transient`, or equivalent flag does not alter the mapping and
never authorizes retry or fallback.

If evidence cannot establish that failure occurred before full send, the
classifier uses the post-send branch. In particular, startup may not infer
`KnownFailed` merely because no durable full-send acknowledgement exists. An
`InFlight` call with neither definitive response evidence nor proof that the
write stopped short is `Ambiguous`. A provider request-status API may later
supply definitive evidence, but absence, lookup failure, or an unrecognized
status does not.

### Timeouts use the boundary but budgets stay open

An observed timeout is not a disposition by itself. Before full send it follows
the applicable pre-send rule; absent the proof-bearing cancellation branch, it
is `KnownFailed`. After full send, or when send completion is uncertain, it is
`Ambiguous` unless a definitive provider response is also available. An explicit
provider timeout error response follows the adapter's exact native response
mapping.

This record selects no timeout budget, clock, deadline source, grace period,
polling duration, cancellation trigger, or resource-limit policy. Adding any
such timer must preserve the classification rule above.

### Existing lifecycle and policy remain authoritative

Classification is serialized with the call and aggregate transition under
ADR-0004 and ADR-0005. A terminal call never reopens. Later resolving evidence
may affect turn-level ambiguity handling without rewriting the physical
`Ambiguous` disposition. Live and startup handling apply the same evidence rule;
startup still ends the abandoned turn attempt `Lost`.

No classification in this record authorizes an adapter retry, a replacement
call, or fallback. ADR-0005's outcome-authority and version-one retry rules and
the open ADR-0006 fallback decision remain unchanged.

## Invariants

- INV-014: every classification applies to one durable model call with its exact
  pinned target and context frontier; provider status does not replace target
  evidence.
- INV-025: loss after a request was or may have been fully sent remains
  representably `Ambiguous` rather than being coerced to failure.
- INV-026: neither a known nor ambiguous classification authorizes automatic
  repetition.
- INV-034: live adapters and startup recovery use the same full-send and
  definitive-response evidence rule while retaining their distinct attempt-end
  dispositions.
- INV-035: credential access failure before send and explicit provider-side
  authentication rejection both preserve the credential boundary and classify
  without exposing credential values.

## Strongest alternative

Classify every provider transport or protocol error as `KnownFailed` and let
provider SDK retryability decide whether orchestration tries again. This is
simple and matches many SDK surfaces.

It is rejected because an error observed after full send may hide a completed
request whose response was lost. SDK retryability is an operational hint, not
evidence about external effect or authority to create another model call.

## Rejected alternatives

- **Treat every `InFlight` failure as ambiguous.** This discards positive
  evidence that request construction or send failed before the full-send
  boundary and would make known preparation failures unnecessarily block the
  session.
- **Infer scripted outcomes from simulated timing.** Test timing is not provider
  evidence and makes fixtures nondeterministic descriptions of lifecycle
  expectations.
- **Use one provider-independent HTTP status taxonomy.** Provider contracts use
  different transports, status vocabularies, and refusal payloads. The shared
  rule is that a recognized refusal takes precedence over a generic terminal
  error and each other explicit terminal response has one exact classification;
  adapters own exhaustive native-status mapping.
- **Call every timeout known failure.** A timeout after full send says only that
  the hub did not receive a definitive response before a local deadline.

## Consequences

Real adapters must observe the full-send boundary, preserve enough evidence to
support their classification, and maintain an exhaustive native-status table.
Unknown post-send outcomes conservatively become ambiguous and may retain the
session slot or require reconciliation under the accepted lifecycle.

Scripted-provider tests are more verbose because every terminal result is
declared, but their expected lifecycle is reviewable without reverse-engineering
fixture timing. The same fixtures can exercise every existing disposition
without pretending to reproduce a real provider.

No dependency, domain state, retry mechanism, or timeout budget is selected.

## Scenario walkthroughs

- **S02:** A scripted response declares its disposition. For a real adapter, an
  applied interrupt with exact proof and evidence that the request was not
  accepted and the interrupt prevented all remaining work cancels before full
  send; other request-preparation or incomplete-send failure is known. A
  recognized refusal takes precedence over a generic terminal error; other
  native responses follow their exact mapping, while loss after full send is
  ambiguous. No partial draft becomes authoritative.
- **S04:** Startup uses definitive provider evidence when available. Otherwise
  an `InFlight` call that may have crossed full send is ambiguous while its
  prior-process attempt ends in the matching `Lost` branch. A request-status API
  may later resolve turn-level uncertainty without rewriting a physically
  ambiguous call.
- **S27:** Provider-target mismatch keeps ADR-0005 and ADR-0031 precedence. This
  record classifies the independently observed provider outcome but does not
  change fatal-cause or exact-ambiguity-set derivation.

## Open questions

- Timeout and resource budgets, their ownership, and their operational defaults
  remain under resource governance.
- Provider-identity normalization, native target-status trust, and detailed
  provenance remain reserved ADR-0007 scope.
- Whether a provider request-status API is used, and any polling or
  reconciliation policy around it, remains adapter and recovery scope.

## Explicit non-decisions

This record does not choose a provider, SDK, HTTP client, persistence or wire
shape, timeout duration, retry, fallback, request-status polling policy,
provider-error presentation taxonomy, or monitoring system. It does not promise
exactly-once provider execution or equate full request send with provider
acceptance.
