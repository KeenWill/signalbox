# OpenAI Responses item state machine

The Responses stream decoder will keep one state per output index, binding its
item id and kind to its content and completion phase. An event outside the
accumulated state will end the exchange with incomplete-stream evidence
(`StreamProtocolViolation`), retaining observed model, usage, recognized finish,
and tool-call facts. Only a consistent protocol terminal will supply completion
or refusal evidence.

| State             | Event                                                                   | Transition                                     |
| ----------------- | ----------------------------------------------------------------------- | ---------------------------------------------- |
| Unseen            | Item snapshot or indexed content event                                  | Open item with the event's id and kind         |
| Open              | Delta                                                                   | Extend the open content part                   |
| Open              | Added snapshot                                                          | Extend the observed prefix                     |
| Open              | Content done                                                            | Freeze that part's type and bytes              |
| Open              | Item done                                                               | Freeze the item and its content layout         |
| Open              | Terminal output                                                         | Close the item against its accumulated content |
| Done              | Repeated done or terminal output                                        | Preserve the frozen item                       |
| Done              | Delta                                                                   | Incomplete-stream evidence                     |
| Any observed item | Missing from terminal output, changed identity, or incompatible content | Incomplete-stream evidence                     |

Content parts have open and done states. Open parts retain delta bytes and the
latest snapshot prefix; done parts retain one exact value. The item state owns
content ordering, so flattened observation indexes derive from the same state
that accepts snapshots. Function arguments occupy one part. Reasoning retains
the exact item-done JSON for replay; an omitted terminal ciphertext uses that
retained item, while a supplied ciphertext must agree.

At `response.incomplete` with `max_output_tokens`, completed and incomplete
items retain their reported content, including incomplete function arguments.
The ceiling does not permit rewriting prior content or omitting observed items.
The shared terminal converter determines typed output-ceiling completion. An
intact failed envelope remains provider-error evidence before ancillary fields
are examined.

| Existing checks                                                     | State that subsumes them                |
| ------------------------------------------------------------------- | --------------------------------------- |
| Separate item-id and item-kind maps                                 | Bound identity of each item             |
| Post-item-done delta guard                                          | Done item transition                    |
| Content type, delta completion, snapshot equality and prefix checks | Open/done content transition            |
| Occupancy changes, stale indexes, completed layout growth           | Item content ordering and frozen layout |
| Completed call id, name, status checks                              | Frozen function item                    |
| Completed reasoning map, repeated bytes, terminal ciphertext checks | Frozen reasoning item with replay bytes |
| Terminal omission of observed indexes or announced calls            | Terminal coverage of item states        |

Response-envelope decoding, reported usage, argument nesting limits, and the
shared buffered/streamed terminal conversion remain responsible for their
existing contracts. No new bounds, configuration, recovery policy, dependencies,
or live-provider tests are introduced.
