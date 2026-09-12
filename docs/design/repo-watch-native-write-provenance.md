# Native review-write provenance

Native GitHub review publication and thread replies will retain the review IDs
returned by their successful mutations in repository-watch's module store.
Reply receipts will also retain the created comment ID. The authenticated login
will not determine suppression.

A transport callback will acquire the existing repository frontier lock before
the mutation and commit its receipt before releasing ingestion. The daemon will
supply the independently authenticated module store to that callback before
admitting tool execution. Failure before dispatch will remain a non-dispatch;
failure to retain an accepted write will return an unknown outcome.

The differ will carry the provider review ID alongside review-submission and
new-thread occurrences. Stored comparison baselines will retain thread state
without creation provenance. The readable-event view will exclude events whose
review ID matches a native receipt; the immutable event table will retain them.
Other reviews by the same login and subsequent thread reopenings will remain
eligible.

This change does not add cross-rule session claims, login filters, configuration,
receipt expiry, or recovery of remote writes whose acknowledgement was lost.
