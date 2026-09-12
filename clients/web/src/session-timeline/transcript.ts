import type {
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from '../generated/web-contract.mjs'
import { readSessionTranscript, type SessionTranscriptLimits } from '../product'
import { SESSION_WINDOW_BYTES } from '../session-workspace'
import { BoundedSessionHistory, HttpSessionTimelineSource, type SessionWindowAnchor } from './model'

// Each retained window has at most eight bounded detail responses; keep three neighboring windows.
export const TRANSCRIPT_WINDOW_ITEMS = 8
export const TRANSCRIPT_RETAINED_WINDOWS = 3

export interface TranscriptWindow {
  window: WebSessionTimelineWindow
  details: readonly WebSessionTimelineDetailPage[]
}

export class TranscriptWindowReader {
  private history: BoundedSessionHistory | undefined

  constructor(private readonly sessionId: string) {}

  async read(
    anchor: SessionWindowAnchor,
    limits: SessionTranscriptLimits,
    signal: AbortSignal,
  ): Promise<TranscriptWindow> {
    this.history ??= new BoundedSessionHistory(
      this.sessionId,
      await HttpSessionTimelineSource.connect((input, init) => fetch(input, init), signal),
    )
    const window = await this.history.load(
      anchor,
      { maxItems: TRANSCRIPT_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
      signal,
    )
    const details = await Promise.all(
      window.items.map((item) =>
        readSessionTranscript(
          this.sessionId,
          item.address.event_sequence,
          item.address.event_sequence,
          null,
          limits,
          signal,
        ),
      ),
    )
    return { window, details }
  }
}
