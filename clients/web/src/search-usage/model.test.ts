import { describe, expect, it, vi } from 'vitest'
import { webContractBootstrapFixture } from '../product.fixture'
import { HttpSearchUsageSource } from './model'
import {
  SEARCH_USAGE_FAR_ADDRESS,
  SEARCH_USAGE_SCENARIO_SESSION_ID,
  SearchUsageScenarioSource,
} from './scenario'

const bootstrap = webContractBootstrapFixture

const searchPage = {
  results: [
    {
      session_id: SEARCH_USAGE_SCENARIO_SESSION_ID,
      projection_id: '1',
      address: { event_sequence: SEARCH_USAGE_FAR_ADDRESS },
      source: {
        kind: 'turn_transcript_entry',
        semantic_entry_id: '00000000-0000-0000-0000-000000000101',
        turn_id: '00000000-0000-0000-0000-000000000102',
      },
      content_class: 'assistant_transcript',
      snippet: 'needle in canonical evidence',
      highlights: [{ start_byte: 0, end_byte: 6 }],
    },
  ],
  continuation: null,
} as const

const usageSummary = { groups: [], truncated: false } as const
const usageCalls = { calls: [], continuation: null } as const

const adapterFixture = {
  searchUrls: [
    '/api/bootstrap',
    `/api/search?strategy=lexical&q=needle&max_items=100&session_id=${SEARCH_USAGE_SCENARIO_SESSION_ID}`,
  ],
  usageUrls: [
    '/api/bootstrap',
    '/api/usage/summary?provenance=reported',
    '/api/usage/calls?order=newest&max_items=100&provenance=reported',
  ],
} as const

const responseFor = (url: string): Response => {
  if (url === '/api/bootstrap') return Response.json(bootstrap)
  if (url.startsWith('/api/search?')) return Response.json(searchPage)
  if (url.startsWith('/api/usage/summary?')) return Response.json(usageSummary)
  if (url.startsWith('/api/usage/calls?')) return Response.json(usageCalls)
  return Response.json(
    { error: { kind: 'application', code: 'unexpected', message: 'unexpected' } },
    { status: 500 },
  )
}

const scriptedFetch = (urls: string[]): typeof fetch =>
  vi.fn(async (input) => {
    const url = String(input)
    urls.push(url)
    return responseFor(url)
  }) as typeof fetch

describe('HttpSearchUsageSource', () => {
  it('uses lexical product parameters for a current-session search', async () => {
    const urls: string[] = []
    const source = await HttpSearchUsageSource.connect(scriptedFetch(urls))
    const page = await source.search({
      text: 'needle',
      scope: { kind: 'session', sessionId: SEARCH_USAGE_SCENARIO_SESSION_ID },
      maxItems: 1_000,
    })

    expect(urls).toEqual(adapterFixture.searchUrls)
    expect(page).toEqual(searchPage)
  })

  it('reads dedicated usage endpoints without requesting transcript material', async () => {
    const urls: string[] = []
    const source = await HttpSearchUsageSource.connect(scriptedFetch(urls))
    await source.usageSummary({ provenance: 'reported' })
    await source.usageCalls({
      filters: { provenance: 'reported' },
      order: 'newest',
      maxItems: 1_000,
    })

    expect(urls).toEqual(adapterFixture.usageUrls)
    expect(urls.some((url) => url.includes('/timeline'))).toBe(false)
  })

  it('rejects highlight offsets that split UTF-8 code points', async () => {
    const request = vi
      .fn()
      .mockResolvedValueOnce(Response.json(bootstrap))
      .mockResolvedValueOnce(
        Response.json({
          results: [
            {
              ...searchPage.results[0],
              snippet: 'évidence',
              highlights: [{ start_byte: 0, end_byte: 1 }],
            },
          ],
          continuation: null,
        }),
      ) as typeof fetch
    const source = await HttpSearchUsageSource.connect(request)

    // The generated decoder now owns the UTF-8 boundary rule and rejects before the adapter's own
    // highlight validation runs, so accept either layer's refusal of a split code point.
    await expect(
      source.search({ text: 'evidence', scope: { kind: 'global' }, maxItems: 10 }),
    ).rejects.toThrow(/invalid highlight bounds|UTF-8 boundaries/)
  })
})

describe('SearchUsageScenarioSource', () => {
  it('keeps its lexical cursor ordering stable across bounded pages', async () => {
    const source = new SearchUsageScenarioSource()
    const first = await source.search({
      text: 'needle',
      scope: { kind: 'global' },
      maxItems: 2,
    })
    const second = await source.search({
      text: 'needle',
      scope: { kind: 'global' },
      maxItems: 2,
      after: first.continuation,
    })

    expect(first.results[0]?.address.event_sequence).toBe(SEARCH_USAGE_FAR_ADDRESS)
    expect(first.continuation?.projection_id).toBe('2')
    expect(second.results[0]?.address.event_sequence).toBe('777751')
  })
})
