# Generated merge verification design

This unbuilt design extends [tool-loop](../spec/tool-loop.md).

## Goal

Permit generated outputs to reflect the combined tree while preserving the
exact-line contract for hand-written files.

## Design

A configured push may replace base-added lines only in outputs declared by
`config/generated-files.json` in the base parent. Each declaration names a
checked-in generator and exact output paths or directory prefixes. The generator
runs from the combined tree with direct executable arguments.

Before publication, the verifier materializes that tree in a disposable
workspace, removes the candidate outputs, runs the declared generators through
the existing sandboxed command runner, and compares each candidate output with
the committed merge content. Missing, failed, or different output refuses
publication.

The Git push boundary supplies generation requests to its injected executor; the
daemon supplies its workspace sandbox configuration and read-only Cargo
registry. Generation carries no push credentials. The snapshot selected before
generation is the snapshot subsequently published.

## Compatibility constraints

Output markers and hand-written fixtures establish no exemption. Other paths
retain the [exact base-line contract](../spec/tool-loop.md). No model-supplied
exemption, generator receipt, new tool, or daemon configuration key is added.

## Acceptance criteria

A declared output matching its generator on the combined tree passes. Different
or absent generated output, a hand-written fixture, and a declaration added only
by the branch are refused before transport.
