import type {
  WebSearchPage,
  WebUsageCallPage,
  WebUsageSummary,
} from '../generated/web-contract.mjs'
import {
  decodeWebSearchPage,
  decodeWebUsageCallPage,
  decodeWebUsageSummary,
} from '../generated/web-contract.mjs'
import type { SearchRequest, SearchUsageSource, UsageCallsRequest, UsageFilters } from './model'

export const SEARCH_USAGE_SCENARIO_SESSION_ID = '00000000-0000-0000-0000-000000000994'
export const SEARCH_USAGE_FAR_ADDRESS = '777777'
const SEARCH_RESULT_COUNT = 72
const USAGE_CALL_COUNT = 240
const MODEL_ALPHA = '00000000-0000-0000-0000-000000001001'
const MODEL_BETA = '00000000-0000-0000-0000-000000001002'
const PROFILE_ALPHA = 'profile-alpha'
const PROFILE_BETA = 'profile-beta'

export interface SearchUsageScenarioDiagnostics {
  searchReads: number
  usageSummaryReads: number
  usageCallReads: number
  transcriptRevealReads: number
}

const uuidAt = (namespace: number, index: number): string =>
  `00000000-0000-0000-${String(namespace).padStart(4, '0')}-${String(index).padStart(12, '0')}`

const searchResult = (query: string, index: number): WebSearchPage['results'][number] => {
  // The contract orders a search page strictly descending by (address, projection id), so the
  // scenario walks addresses down from the far address rather than up from it.
  const address = String(Number(SEARCH_USAGE_FAR_ADDRESS) - index * 13)
  const contentClass = index % 9 === 0 ? 'derived_text_artifact' : 'assistant_transcript'
  const snippet = `${query} — bounded lexical evidence at durable address ${address}`
  return {
    session_id: SEARCH_USAGE_SCENARIO_SESSION_ID,
    // The contract anchors a search continuation to the final result's projection id, so the
    // scenario numbers projections one past the index the pager slices on.
    projection_id: String(index + 1),
    address: { event_sequence: address },
    source:
      contentClass === 'derived_text_artifact'
        ? { kind: 'derived_artifact', artifact_id: uuidAt(994, index + 1) }
        : {
            kind: 'turn_transcript_entry',
            semantic_entry_id: uuidAt(995, index + 1),
            turn_id: uuidAt(996, Math.floor(index / 3) + 1),
          },
    content_class: contentClass,
    snippet,
    highlights: [{ start_byte: 0, end_byte: new TextEncoder().encode(query).byteLength }],
  }
}

const tokenValue = (value: number | null): string | null => (value === null ? null : String(value))

const usageCall = (index: number): WebUsageCallPage['calls'][number] => {
  const reported = index % 3 !== 1
  const modelId = index % 2 === 0 ? MODEL_ALPHA : MODEL_BETA
  const missingOutput = index % 11 === 0
  const meteredEquivalent = index % 4 === 3
  const rateVersion = index % 5 === 0 ? 'rates-2026-08-b' : 'rates-2026-08-a'
  return {
    call_id: uuidAt(997, index + 1),
    call_kind: index % 7 === 0 ? 'approval_judge' : 'model_call',
    session_id: SEARCH_USAGE_SCENARIO_SESSION_ID,
    turn_id: uuidAt(998, Math.floor(index / 2) + 1),
    model_id: modelId,
    recorded_at_micros: String(1_787_400_000_000_000 - index * 1_000_000),
    provenance: reported ? 'reported' : 'estimated',
    input_semantics: 'cache_exclusive',
    tokens: {
      input: String(800 + index),
      output: tokenValue(missingOutput ? null : 120 + (index % 31)),
      cache_creation_input: null,
      cache_read_input: tokenValue(index % 4 === 0 ? 300 + index : null),
    },
    cost: missingOutput
      ? { status: 'unavailable', reason: 'configuration_unavailable' }
      : {
          status: 'derived',
          amount_usd: index % 5 === 0 ? '0.031' : '0.017',
          rate_version: rateVersion,
          label: meteredEquivalent ? 'metered_equivalent' : 'real',
        },
  }
}

const usageGroups = (): WebUsageSummary['groups'] => [
  {
    call_kind: 'model_call',
    model_id: MODEL_ALPHA,
    profile_id: PROFILE_ALPHA,
    provenance: 'reported',
    input_semantics: 'cache_exclusive',
    coverage: {
      input: true,
      output: true,
      cache_creation_input: false,
      cache_read_input: false,
    },
    call_count: '96',
    tokens: {
      input: '91200',
      output: '14280',
      cache_creation_input: null,
      cache_read_input: null,
    },
    cost: {
      status: 'derived',
      amount_usd: '1.73',
      rate_version: 'rates-2026-08-a',
      label: 'real',
    },
  },
  {
    call_kind: 'model_call',
    model_id: MODEL_BETA,
    profile_id: PROFILE_BETA,
    provenance: 'estimated',
    input_semantics: 'cache_exclusive',
    coverage: {
      input: true,
      output: true,
      cache_creation_input: false,
      cache_read_input: false,
    },
    call_count: '64',
    tokens: {
      input: '60800',
      output: '9400',
      cache_creation_input: null,
      cache_read_input: null,
    },
    cost: {
      status: 'derived',
      amount_usd: '1.11',
      rate_version: 'rates-2026-08-b',
      label: 'metered_equivalent',
    },
  },
  {
    call_kind: 'approval_judge',
    model_id: MODEL_ALPHA,
    profile_id: PROFILE_ALPHA,
    provenance: 'reported',
    input_semantics: 'cache_exclusive',
    coverage: {
      input: true,
      output: false,
      cache_creation_input: false,
      cache_read_input: true,
    },
    call_count: '18',
    tokens: {
      input: '14500',
      output: null,
      cache_creation_input: null,
      cache_read_input: '4100',
    },
    cost: { status: 'unavailable', reason: 'configuration_unavailable' },
  },
]

const matchesUsageFilters = (
  call: WebUsageCallPage['calls'][number],
  filters: UsageFilters,
): boolean =>
  (!filters.sessionId || call.session_id === filters.sessionId) &&
  (!filters.turnId || call.turn_id === filters.turnId) &&
  (!filters.modelId || call.model_id === filters.modelId) &&
  (!filters.provenance || call.provenance === filters.provenance) &&
  (!filters.callKind || call.call_kind === filters.callKind)

export class SearchUsageScenarioSource implements SearchUsageSource {
  readonly limits = {
    max_search_query_bytes: 512,
    max_search_page_items: 100,
    max_search_snippet_bytes: 512,
    max_usage_aggregate_groups: 256,
    max_usage_call_page_items: 100,
  }
  private readonly counts: SearchUsageScenarioDiagnostics = {
    searchReads: 0,
    usageSummaryReads: 0,
    usageCallReads: 0,
    transcriptRevealReads: 0,
  }

  get diagnostics(): SearchUsageScenarioDiagnostics {
    return { ...this.counts }
  }

  noteTranscriptReveal(): void {
    this.counts.transcriptRevealReads += 1
  }

  async search(request: SearchRequest): Promise<WebSearchPage> {
    this.counts.searchReads += 1
    const start = Number(request.after?.projection_id ?? '0')
    const end = Math.min(start + request.maxItems, SEARCH_RESULT_COUNT)
    const results = Array.from({ length: end - start }, (_, offset) =>
      searchResult(request.text, start + offset),
    )
    return decodeWebSearchPage({
      results,
      continuation:
        end < SEARCH_RESULT_COUNT
          ? {
              address: results.at(-1)?.address,
              projection_id: String(end),
            }
          : null,
    })
  }

  async usageSummary(filters: UsageFilters): Promise<WebUsageSummary> {
    this.counts.usageSummaryReads += 1
    const groups = usageGroups().filter(
      (group) =>
        (!filters.modelId || group.model_id === filters.modelId) &&
        (!filters.provenance || group.provenance === filters.provenance) &&
        (!filters.callKind || group.call_kind === filters.callKind),
    )
    return decodeWebUsageSummary({ groups, truncated: false })
  }

  async usageCalls(request: UsageCallsRequest): Promise<WebUsageCallPage> {
    this.counts.usageCallReads += 1
    const calls = Array.from({ length: USAGE_CALL_COUNT }, (_, index) => usageCall(index)).filter(
      (call) => matchesUsageFilters(call, request.filters),
    )
    const start = request.after
      ? Math.max(calls.findIndex((call) => call.call_id === request.after?.call_id) + 1, 0)
      : 0
    const page = calls.slice(start, start + request.maxItems)
    const end = start + page.length
    return decodeWebUsageCallPage(
      {
        calls: page,
        continuation:
          end < calls.length
            ? {
                call_id: page.at(-1)?.call_id,
                recorded_at_micros: page.at(-1)?.recorded_at_micros,
              }
            : null,
      },
      request.order,
    )
  }
}
