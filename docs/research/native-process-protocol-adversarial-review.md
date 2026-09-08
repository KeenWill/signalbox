# Native process-protocol adversarial review

## Scope and method

This review covers the shipping request client, wire decoder, transcript
projector, synchronization reducer and driver, mutation service, and mutation
retry state in the native views. The Swift workflow includes
`crates/process-protocol/**` in its native change classifier, so protocol edits
select native validation.

Verification below means source inspection and a concrete execution trace.
Native tests require the macOS CI environment; the traces are not claims of
executed concurrency tests. P2 denotes a bounded correctness defect with a
specific trigger, without an established production occurrence.

## Findings

### P2: stop cleanup can cancel a concurrent restart

Verified in
[SessionSynchronizationDriver.swift](../../clients/native/Sources/SignalboxClient/SessionSynchronizationDriver.swift),
`stop`, `process`, `drainInputs`, and `cancelAllWork`.

`stop` sets `isStarted` false before awaiting the queued stop input, then calls
`cancelAllWork` outside that queue. A concurrent `start` can set `isStarted`
true and queue a start while the stop effects suspend. The drain resumes the
stop caller and can begin executing the queued start; if the stop caller resumes
after the new follow or deadline task is installed, its unconditional cleanup
cancels the new generation's work. The reducer can remain in `connect` with
neither a transport nor its deadline.

The drain awaits the old transport closure before processing the queued start,
so a regression cannot gate that closure on observing the restart's connect
update. Reproducing the cleanup race requires controlling when the stop caller
resumes after the queued stop completes. No deterministic runtime reproduction
is established here. Keep stop cleanup in the serialized input effects, or bind
cleanup to the stopped generation.

### P2: an uncorrelated mutation receipt discards retry identity

Verified in
[ProcessService.swift](../../clients/native/Sources/SignalboxClient/ProcessService.swift),
`mutation` and `submit`, and
[ProcessViews.swift](../../clients/native/Sources/SignalboxApp/ProcessViews.swift),
`retainsPreparedMutationIdentity` and the submission failure handler.

After dispatch, an unrelated recognized response produces `unexpectedMessage`. A
receipt of the expected kind but naming another session also produces that error
in `submit`. Neither response proves whether the intended mutation committed.
The view treats `unexpectedMessage` as permitting the prepared command identity
to be discarded. Retrying unchanged input can therefore mint a new command
identity after an unresolved dispatch.

A deterministic regression can return a correctly framed response with the
expected request identity but a receipt for another session, then repeat the
same input. The retry must retain the original durable command identity and
report unresolved outcome. Treat post-dispatch receipt-correlation failures as
ambiguous for identity retention; keep pre-dispatch failures separate.

## Verified protections and limits

- [ProcessProtocolClient.swift](../../clients/native/Sources/SignalboxClient/ProcessProtocolClient.swift)
  checks response request identity and protocol version, closes failed
  exchanges, serializes reads, and distinguishes failure before sending from an
  unknown send outcome. Frame assembly rejects unterminated oversized input.
- [ProcessProtocol.swift](../../clients/native/Sources/SignalboxModels/ProcessProtocol.swift)
  runs the duplicate-member scanner before typed decoding. The wire envelope
  rejects unadmitted fields and uses canonical identity types.
- [SessionSynchronization.swift](../../clients/native/Sources/SignalboxClient/SessionSynchronization.swift)
  checks generation and refresh identity before applying callbacks. The FIFO in
  the driver serializes reducer effects; the first finding concerns cleanup
  outside that FIFO.
- [ProcessTranscriptProjector.swift](../../clients/native/Sources/SignalboxClient/ProcessTranscriptProjector.swift)
  projects into a candidate copy and assigns it only after validation. Side
  snapshots require evidence for the triggering event, and tool results require
  a correlated request.
- The mutation service reuses the prepared request through its bounded ambiguity
  retries. Native view state retains the prepared identity after cancellation
  and retry exhaustion; the second finding concerns receipt-correlation errors.

The existing
[client tests](../../clients/native/Tests/SignalboxClientTests/ProcessProtocolClientTests.swift)
and
[synchronization tests](../../clients/native/Tests/SignalboxClientTests/SessionSynchronizationTests.swift)
cover send cancellation, framing, stale completions, and reducer stop behavior.
They do not establish the two execution traces above. The fixes and their
regressions are follow-up implementation work; this report changes no wire
contract, synchronization policy, or native runtime behavior.
