# Ownership seam design

This design is not built; it extends the
[ownership seam](../spec/ownership-seam.md).

## Goal

Deliver durable, checked rule activation from core to the owning module.

## Design

The reload-intent input family carries the reload command identity, checked
per-repository rule sets, and rule-set digest. Core delivers rule activation
from the retained intent, and the module handles repeated delivery idempotently.
[Process protocol](process-protocol.md) owns intent persistence and recovery.

## Compatibility constraints

Delivery grants neither role access to the other's tables.

## Acceptance criteria

Repeated delivery applies the retained payload once, even if configuration files
change.
