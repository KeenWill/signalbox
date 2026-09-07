# Ownership seam design

This design is not built; it extends the
[ownership seam](../spec/ownership-seam.md).

## Goal

Deliver durable, checked reload intent from core to the owning module.

## Design

The reload-intent input family carries the reload command identity, checked
per-repository rule sets, convergence targets, and rule-set digest. Core
delivers the retained payload, and the module handles repeated delivery
idempotently. [Process protocol](process-protocol.md) owns intent persistence
and recovery.

## Compatibility constraints

Delivery grants neither role access to the other's tables.

## Acceptance criteria

Repeated delivery applies the retained payload once, even if configuration files
change.
