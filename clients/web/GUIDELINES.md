# Signalbox web client guidelines

These rules apply to every user-facing web change.

## Layout

- The session workspace feels like a normal chat application while preserving
  Signalbox's expert density.
- The transcript and composer form the primary page content.
- Only session identity, prominent session facts, and controls that directly
  operate on the conversation sit above the transcript.
- Toolbars omit labels, status, and controls that do not help with the current
  task.
- Live status and refresh actions share the relevant surface header instead of
  occupying a separate bar.
- Timeline history uses virtualized continuous scrolling over bounded keyset
  pages.
- Paging controls never occupy the transcript layout.
- First and latest timeline actions remain compact navigation controls.
- Primary navigation collapses to an icon rail on wide screens and a drawer on
  narrow screens.
- Inspectors collapse into a pane, sheet, or dedicated route without displacing
  the conversation unnecessarily.

## Information hierarchy

- Every surface presents its summary before its detail.
- Results level shows each turn's user message and final response.
- Results level hides intermediate messages, thinking, tool calls, and telemetry
  detail.
- Condensed level adds meaningful progress, tool summaries, warnings, and
  compact telemetry.
- Condensed level hides raw payloads and routine internal events.
- Full level exposes supported messages, thinking, tool calls, telemetry, and
  event detail in document flow.
- Selecting a turn opens that turn's deeper detail without expanding unrelated
  turns.
- Deep provenance and audit facts live in an inspector or disclosure below the
  summary.

## Navigation

- Every row, chip, search result, attention item, and artifact reference
  navigates to the thing it names.
- Every detail surface offers a route back to its containing session or
  collection.
- Session, turn, artifact, and search identifiers travel in the URL and command
  palette.
- Forms never ask users to enter identifiers, digests, or media types to locate
  existing content.
- The command palette exposes direct navigation to addressable sessions, turns,
  artifacts, and product surfaces.
- Session navigation exposes a clear new-session action.
- Session surfaces expose a clear rename action.
- Search covers titles and transcript text.
- Search results open the matching content at its precise location.
- Development scenarios remain outside production navigation.
- The product wordmark either navigates home or is omitted.

## Vocabulary

- Every user-facing wire value receives its product label from
  [`src/labels.ts`](src/labels.ts).
- Components never maintain a second label map or derive labels by rewriting
  wire values.
- Hand-written copy uses plain product language.
- User-facing copy avoids daemon, storage, protocol, and projection terminology.
- Existing product terms remain consistent across navigation, headings,
  controls, states, and detail views.

The following replacements apply to existing user-facing copy.

| Existing copy                                                                                        | Product copy                                                   |
| ---------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| `Accepted input`                                                                                     | `You` in the transcript eyebrow and `Your message` in detail   |
| `Dispatch` / `Dispatch details`                                                                      | `Started by` / `Trigger details`                               |
| `Provenance` / `Provenance session` / `Provenance turn` / `Provenance command`                       | `Source` / `Source session` / `Source turn` / `Source command` |
| `Frontier ID`                                                                                        | `Position`, or omit it when it is not needed                   |
| `Result ID` / `Final ID`                                                                             | Omit outside debugging views                                   |
| `Compaction` / `Compaction summary` / `Summary entry`                                                | `Context trim` / `What was kept` / `Summary`                   |
| `Injection settlement`                                                                               | `Message inserted`                                             |
| `Delegation content` / `Child session` / `Relationship`                                              | `Sub-agent message` / `Sub-agent` / `Link`                     |
| `Per-call override` / `Settings precedence` / `Defaults version` / `Adjustments` / `Caller override` | One `Model settings` disclosure                                |
| `Request context items`                                                                              | `Messages sent to the model`                                   |
| `Placement revision`                                                                                 | Omit                                                           |
| `Member index`                                                                                       | Omit                                                           |
| `Observed through`                                                                                   | `Up to date as of`                                             |
| `Text excerpt · byte N of M` / `From byte N of M`                                                    | `Showing part of a long message` with a `Show all` control     |
| `Digest` / `Declared media type`                                                                     | `File hash` / `File type` when shown in detail                 |
| `Paged snapshot` / `Live monitor` / `Monitor paused` / `Monitor unavailable`                         | `Snapshot` / `Live` / `Paused` / `Disconnected`                |

## Rendering

- Every known tool kind uses a typed renderer with a compact summary and detail
  on demand.
- Tool summaries show the product action, current state, and meaningful result
  instead of a generic payload dump.
- Tool arguments, results, failures, and metadata appear in structured product
  views when their shapes are known.
- Raw JSON and raw protocol values remain behind an explicit raw-view toggle.
- Unknown tool kinds receive a safe product-language summary with inspectable
  raw detail.
- Supported images, audio, video, and documents render inline with accessible
  names and ordinary open or download actions.
- Artifact and attachment details open from the content reference that named
  them.
- Media rendering preserves the original artifact while presenting available
  typed views.

## Session facts

- The session title is the primary session heading.
- Session summaries distinguish sessions with meaningful titles or available
  trigger, repository, and activity context.
- The associated pull request is prominent and navigates to its product view.
- The head branch is prominent, and the base branch appears when it adds useful
  context.
- The initiating trigger is prominent and uses product language.
- The current session state is prominent.
- The last activity time is prominent.
- Total session cost is prominent whenever pricing is available.
- Turn and model-call costs appear with their corresponding summaries whenever
  pricing is available.
- Attention and session-list summaries show session cost whenever pricing is
  available and cost helps compare rows.

## Styling

- Feature-specific styles live in a co-located CSS file imported by the owning
  feature.
- Shared shell styles and tokens remain in the shared stylesheet owned by the
  application shell.
- Every text-entry control uses the shared field primitive.
- The client uses one shared type scale across product surfaces.
- Layout gaps, padding, and control dimensions use shared spacing tokens.
- Fields, labels, controls, and tabular facts align to a consistent grid.
- The composer reads as the transcript's primary text-entry control and keeps
  its main action visually clear.
- Surface hierarchy uses typography, spacing, and subtle separators instead of
  nested decorative cards.
- Semantic color is reserved for status, urgency, selection, focus, and
  provenance.

## How to check a change

- The package `lint`, `check`, `test`, and `build` scripts pass from
  `clients/web`.
- The Playwright browser tests pass for every surface the change touches.
- The affected surface passes visual inspection at its supported wide and narrow
  layouts for hierarchy, alignment, wrapping, clipping, focus, hover, and
  selection.
- The pull-request body includes a current screenshot or Playwright artifact
  path for every visual change.
