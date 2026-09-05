# Conversation import design

This document describes work that is not built; it extends
[conversation-import](../spec/conversation-import.md).

## Goal

Imported entries can carry the usage a source attested, priced at read time and
never stored as money. A converter can consume a blob-backed source directly
from the blob substrate as a stream, so a source need not be assembled in memory
before conversion.

## Design

Usage on an imported entry is source attestation like every other field: the
entry records the counts the source attested, in the same three-way attestation
vocabulary the spec page describes, and records nothing the source did not
state. No dollar amount is persisted. Pricing is keyed by the configured target
the source names, because one provider-model spelling can belong to several
targets carrying different rates. A reader prices an entry only when its
attestation names exactly one configured target, and then selects that target's
rate window covering the entry's attested source timestamp on the `api` channel,
under the same pricing contract native calls use in
[configuration-and-credentials](../spec/configuration-and-credentials.md). Usage
whose source names no configured target, or whose timestamp no window of that
target covers, is unpriced and yields no figure rather than zero. An imported
entry names no credential profile, so a derived figure is an equivalent, never a
billed amount.

Blob-backed conversion adds a second way to supply source bytes to
`ImportedConversationConverter`. The caller names a verified blob; the converter
reads the blob's bytes from the blob substrate as a stream and normalizes
records as they arrive. The result is the same checked aggregate the
whole-source path produces, with the same identity, raw records, entries, and
frontiers for the same bytes. The terminal single-file path and the chunked
socket path keep their bounded whole-source conversion.

## Compatibility constraints

No present surface supplies a blob-backed source to a converter, and no imported
entry carries usage; the content vocabulary has no usage variant.

A streaming path must not reinterpret an existing converter version. Emitting
usage entries from a source that today yields none changes the entry sequence,
so it requires a new converter version; stored snapshots keep their version's
interpretation, and the reconstitution seam re-derives them unchanged.

The converter seam stays byte-oriented: a blob-backed source adds no filesystem
or store types to the domain or application crates. The source digest stays a
function of the format and the raw record sequence alone; transport does not
participate.

A blob-backed read follows the blob contract in
[blob-storage](../spec/blob-storage.md): the bytes are verified against the
recorded hash, and no database transaction spans the store read.

## Acceptance criteria

The same source bytes imported through the terminal path and through a
blob-backed path resolve to one imported conversation with one identity. Peak
memory of a blob-backed import is the aggregate it returns plus a bounded read
buffer; the whole source is never held in memory at once. Every stored snapshot
still reconstitutes under its recorded converter version with no change. An
imported entry that carries usage exposes a cost at read time from the window
its named target and attested timestamp select, and no column holds a dollar
amount; an entry naming no configured target exposes none. Errors and logs from
either path still carry only classes, counts, and daemon-generated identifiers.
