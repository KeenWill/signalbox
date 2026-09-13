import type {
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from '../generated/web-contract.mjs'
import { readSessionTranscript, type SessionTranscriptLimits } from '../product'
import { SESSION_WINDOW_BYTES } from '../session-workspace'
import {
  BoundedSessionHistory,
  HttpSessionTimelineSource,
  initialDetailFacts,
  type InitialDetailFacts,
  sameInitialDetailFacts,
  type SessionWindowAnchor,
} from './model'

// Share one detail budget across each window; keep three neighboring windows.
export const TRANSCRIPT_WINDOW_ITEMS = 8
export const TRANSCRIPT_WINDOW_BYTES = 65536
export const TRANSCRIPT_RETAINED_WINDOWS = 3

export interface TranscriptWindow {
  window: WebSessionTimelineWindow
  details: readonly WebSessionTimelineDetailPage[]
}

export type TranscriptReadAnchor = SessionWindowAnchor & {
  detailLimits?: SessionTranscriptLimits
}

export class TranscriptWindowReader {
  private history: BoundedSessionHistory | undefined
  private readonly facts = new Map<string, InitialDetailFacts>()

  constructor(private readonly sessionId: string) {}

  async read(
    anchor: TranscriptReadAnchor,
    limits: SessionTranscriptLimits,
    signal: AbortSignal,
  ): Promise<TranscriptWindow> {
    this.history ??= new BoundedSessionHistory(
      this.sessionId,
      await HttpSessionTimelineSource.connect((input, init) => fetch(input, init), signal),
    )
    const selected = anchor.detailLimits ?? limits
    const maxItems = Math.min(TRANSCRIPT_WINDOW_ITEMS, selected.max_timeline_detail_items)
    const maxBytes = Math.min(TRANSCRIPT_WINDOW_BYTES, selected.max_timeline_detail_bytes)
    const window = await this.history.load(
      anchor,
      {
        maxItems: Math.min(maxItems, Math.floor(maxBytes / selected.min_timeline_detail_bytes)),
        maxBytes: SESSION_WINDOW_BYTES,
      },
      signal,
    )
    const details = await Promise.all(
      window.items.map((item) =>
        readSessionTranscript(
          this.sessionId,
          item.address.event_sequence,
          item.address.event_sequence,
          null,
          {
            ...selected,
            max_timeline_detail_items: Math.floor(maxItems / window.items.length),
            max_timeline_detail_bytes: Math.floor(maxBytes / window.items.length),
          },
          signal,
        ),
      ),
    )
    const facts = details.flatMap((page) =>
      page.items.map((item) => [item.address.event_sequence, initialDetailFacts(item)] as const),
    )
    for (const [sequence, current] of facts) {
      const previous = this.facts.get(sequence)
      if (previous !== undefined && !sameInitialDetailFacts(previous, current))
        throw new TypeError('Transcript detail changed its retained immutable facts')
    }
    for (const [sequence, current] of facts) {
      const previous = this.facts.get(sequence)
      const retained = previous
        ? {
            immutable: current.immutable,
            excerpts: [
              ...previous.excerpts.filter(
                (prior) => !current.excerpts.some((excerpt) => excerpt.path === prior.path),
              ),
              ...current.excerpts.map((excerpt) => {
                const prior = previous.excerpts.find((candidate) => candidate.path === excerpt.path)
                return prior && prior.text.length > excerpt.text.length ? prior : excerpt
              }),
            ],
          }
        : current
      this.facts.delete(sequence)
      this.facts.set(sequence, retained)
    }
    while (this.facts.size > TRANSCRIPT_RETAINED_WINDOWS * TRANSCRIPT_WINDOW_ITEMS) {
      const oldest = this.facts.keys().next().value
      if (oldest !== undefined) this.facts.delete(oldest)
    }
    return { window, details }
  }
}
