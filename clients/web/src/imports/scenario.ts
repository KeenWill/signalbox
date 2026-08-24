import type {
  WebImportContinuationRequest,
  WebImportContinuationResponse,
  WebImportDescriptor,
  WebImportEntryWindow,
  WebImportEntryWindowRequest,
  WebImportedContentKind,
  WebImportedEntry,
  WebImportedSpeakerEvidence,
  WebImportFormat,
  WebImportListPage,
  WebImportListRequest,
  WebImportSummary,
} from '../generated/web-contract.mjs'
import type { ImportApi } from './api'

// Hard scenario ceiling mirrors the server contract so deterministic tests cannot mask overfetch.
export const SCENARIO_IMPORT_LIST_ITEMS = 100
// Hard scenario ceiling mirrors the server contract's selected imported-entry region.
export const SCENARIO_IMPORT_WINDOW_ITEMS = 101
// Scale proof: the scenario synthesizes catalog rows by keyset without allocating this inventory.
export const SCENARIO_IMPORT_TOTAL = 1_000_000
// Scale proof: the selected synthetic import is far larger than one browser window.
export const SCENARIO_ENTRY_TOTAL = 250_000

const fixtureUuid = (value: number): string =>
  `00000000-0000-7000-8000-${String(value).padStart(12, '0')}`

const formatAt = (index: number): WebImportFormat => {
  switch (index % 3) {
    case 0:
      return 'claude_code_session_jsonl_v2'
    case 1:
      return 'codex_rollout_jsonl_v1'
    default:
      return 'claude_code_session_jsonl_v1'
  }
}

const sourceSessionAt = (index: number): string | undefined =>
  index % 4 === 0 ? `source-session-${Math.floor(index / 4)}` : undefined

const summaryAt = (index: number): WebImportSummary => ({
  imported_conversation_id: fixtureUuid(index + 1),
  display_title: index % 5 === 0 ? undefined : `Imported investigation ${index + 1}`,
  format: formatAt(index),
  source_session_id: sourceSessionAt(index)
    ? { leading_text: sourceSessionAt(index) ?? '', completeness: 'complete' }
    : undefined,
  entry_count: index === 0 ? SCENARIO_ENTRY_TOTAL : 120 + (index % 8_000),
})

const cursorIndex = (cursor: string | null | undefined): number => {
  if (!cursor) return 0
  const match = /-(\d{12})$/.exec(cursor)
  if (!match?.[1]) return 0
  const parsed = Number(match[1])
  return Number.isSafeInteger(parsed) ? parsed : 0
}

const matchesFilters = (summary: WebImportSummary, request: WebImportListRequest): boolean =>
  (request.format === undefined || request.format === null || summary.format === request.format) &&
  (request.source_session_id === undefined ||
    request.source_session_id === null ||
    summary.source_session_id?.leading_text === request.source_session_id)

const contentKindAt = (position: number): WebImportedContentKind => {
  if (position % 17 === 0) return 'source_event'
  if (position % 11 === 0) return 'tool_result'
  if (position % 7 === 0) return 'tool_call'
  if (position % 5 === 0) return 'thinking'
  return 'text'
}

const speakerAt = (position: number): WebImportedSpeakerEvidence => {
  if (position % 13 === 0) return 'not_attested'
  return position % 2 === 0 ? 'assistant' : 'user'
}

const entryAt = (conversation: string, position: number): WebImportedEntry => {
  const contentKind = contentKindAt(position)
  return {
    frontier: {
      imported_conversation_id: conversation,
      imported_entry_id: fixtureUuid(2_000_000 + position),
      position,
    },
    raw_record_position: Math.ceil(position / 3),
    record_entry_position: ((position - 1) % 3) + 1,
    source_speaker: speakerAt(position),
    content_kind: contentKind,
    text:
      contentKind === 'text'
        ? {
            kind: 'attested',
            leading_text: `Synthetic imported source evidence at immutable position ${position}.`,
            completeness: 'complete',
          }
        : undefined,
  }
}

export class ScenarioImportApi implements ImportApi {
  private logicalTotal = SCENARIO_IMPORT_TOTAL
  private nextSessionIdentity: number | null = null
  private readonly continuationSessions = new Map<string, string>()

  injectConcurrentImport(): void {
    this.logicalTotal += 1
  }

  async list(request: WebImportListRequest): Promise<WebImportListPage> {
    const requested = request.limit ?? SCENARIO_IMPORT_LIST_ITEMS
    const limit = Math.min(Math.max(Math.trunc(requested), 1), SCENARIO_IMPORT_LIST_ITEMS)
    const sourceSessionMatch = /^source-session-(\d+)$/.exec(request.source_session_id ?? '')
    if (request.source_session_id !== undefined && request.source_session_id !== null) {
      const sourceIndex = sourceSessionMatch?.[1] ? Number(sourceSessionMatch[1]) * 4 : -1
      const candidate = sourceIndex >= 0 ? summaryAt(sourceIndex) : undefined
      const afterIndex = cursorIndex(request.after)
      const items =
        candidate && sourceIndex >= afterIndex && sourceIndex < this.logicalTotal
          ? matchesFilters(candidate, request)
            ? [candidate]
            : []
          : []
      return { items, next_cursor: undefined }
    }
    const items: WebImportSummary[] = []
    let index = Math.min(cursorIndex(request.after), this.logicalTotal)
    while (index < this.logicalTotal && items.length <= limit) {
      const candidate = summaryAt(index)
      if (matchesFilters(candidate, request)) items.push(candidate)
      index += 1
    }
    const hasMore = items.length > limit
    if (hasMore) items.pop()
    return {
      items,
      next_cursor: hasMore ? items.at(-1)?.imported_conversation_id : undefined,
    }
  }

  async descriptor(importedConversationId: string): Promise<WebImportDescriptor> {
    const index = Math.max(cursorIndex(importedConversationId) - 1, 0)
    const summary = summaryAt(index)
    const latestPosition = summary.entry_count
    return {
      imported_conversation_id: summary.imported_conversation_id,
      display_title: summary.display_title,
      raw_record_count: Math.ceil(latestPosition / 3),
      entry_count: latestPosition,
      source: {
        format: summary.format,
        source_digest_sha256: String(index + 1).padStart(64, '0'),
        source_session_id: summary.source_session_id,
      },
      sizes: {
        raw_source_bytes: latestPosition * 384,
        normalized_source_record_bytes: latestPosition * 128,
        normalized_entry_bytes: latestPosition * 192,
      },
      timeline: {
        first: entryAt(summary.imported_conversation_id, 1).frontier,
        latest: entryAt(summary.imported_conversation_id, latestPosition).frontier,
      },
    }
  }

  async entries(
    importedConversationId: string,
    request: WebImportEntryWindowRequest,
  ): Promise<WebImportEntryWindow> {
    const descriptor = await this.descriptor(importedConversationId)
    const anchor = request.anchor ?? 'first'
    const anchorPosition =
      anchor === 'latest'
        ? descriptor.entry_count
        : anchor === 'position'
          ? (request.position ?? 1)
          : 1
    const before = request.before ?? 25
    const after = request.after ?? 25
    const firstPosition = Math.max(anchorPosition - before, 1)
    const lastPosition = Math.min(anchorPosition + after, descriptor.entry_count)
    const count = Math.min(lastPosition - firstPosition + 1, SCENARIO_IMPORT_WINDOW_ITEMS)
    const items = Array.from({ length: count }, (_, offset) =>
      entryAt(importedConversationId, firstPosition + offset),
    )
    return {
      anchor_position: anchorPosition,
      first_position: firstPosition,
      last_position: firstPosition + items.length - 1,
      has_before: firstPosition > 1,
      has_after: firstPosition + items.length - 1 < descriptor.entry_count,
      items,
    }
  }

  async continueImport(
    _importedConversationId: string,
    request: WebImportContinuationRequest,
  ): Promise<WebImportContinuationResponse> {
    let sessionId = this.continuationSessions.get(request.command_id)
    if (sessionId === undefined) {
      const sessionIdentity = this.nextSessionIdentity ?? 9_000_000 + request.frontier.position
      sessionId = fixtureUuid(sessionIdentity)
      this.nextSessionIdentity = sessionIdentity + 1
      this.continuationSessions.set(request.command_id, sessionId)
    }
    return {
      command_id: request.command_id,
      session_id: sessionId,
      frontier: request.frontier,
      relationship: request.relationship,
    }
  }
}
