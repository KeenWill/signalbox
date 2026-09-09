# Bounded tool failure context

Tool attempts retain exact failure evidence and a separate bounded detail for
model context. Provider rendering keeps the typed failure kind and uses the
bounded detail. Successful result text and failure details consume the same
per-result allowance, including JSON escaping and truncation markers.

Result admission reserves the next response's output, the following call's
output ceiling, and enough envelopes and empty-prefix markers for the maximum
admitted next tool batch. The batch ceiling remains the domain's existing
admission bound. No new policy or ceiling is introduced.
