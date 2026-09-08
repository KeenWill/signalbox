import { describe, expect, it } from 'vitest'
import { searchResultIdentity, tokenSummary, usageGroupIdentity } from './SearchUsage'
import { SearchUsageScenarioSource } from './search-usage/scenario'

describe('search and usage projection identity', () => {
  it('distinguishes projections of the same source kind at one address', async () => {
    const page = await new SearchUsageScenarioSource().search({
      text: 'needle',
      scope: { kind: 'global' },
      maxItems: 4,
    })
    const left = page.results[2]
    const right = page.results[3]
    if (!left || !right) throw new Error('Missing shared-address fixture')
    expect(left.address).toEqual(right.address)
    expect(left.source.kind).toBe(right.source.kind)
    expect(searchResultIdentity(left)).not.toBe(searchResultIdentity(right))
  })

  it('keeps distinct aggregation compatibility classes under separate keys', async () => {
    const summary = await new SearchUsageScenarioSource().usageSummary({})
    const group = summary.groups[0]
    if (!group) throw new Error('Missing usage group')
    const variants = [
      group,
      { ...group, profile_id: 'exact:another-profile' },
      { ...group, input_semantics: 'cache_inclusive' as const },
      { ...group, coverage: { ...group.coverage, cache_creation_input: true } },
    ]
    expect(new Set(variants.map(usageGroupIdentity)).size).toBe(variants.length)
    expect(usageGroupIdentity({ ...group, call_count: '1000' })).toBe(usageGroupIdentity(group))
  })

  it('retains an aggregate key when a refetch changes derived cost metadata', async () => {
    const summary = await new SearchUsageScenarioSource().usageSummary({})
    const group = summary.groups[0]
    if (!group || group.cost.status !== 'derived') throw new Error('Missing derived usage group')
    const costs: (typeof group)['cost'][] = [
      { ...group.cost, label: 'metered_equivalent' },
      { ...group.cost, rate_version: 'refetched-rates' },
      { ...group.cost, amount_usd: '2.50' },
      { status: 'unavailable', reason: 'configuration_unavailable' },
      { status: 'unavailable', reason: 'no_token_evidence' },
    ]
    for (const cost of costs) {
      expect(usageGroupIdentity({ ...group, cost })).toBe(usageGroupIdentity(group))
    }
  })

  it('renders cache creation and cache read as separate token evidence', () => {
    expect(
      tokenSummary({
        input: '10',
        output: null,
        cache_creation_input: '20',
        cache_read_input: '30',
      }),
    ).toBe('in 10 · out — · cache write 20 · cache read 30')
  })

  it('keeps scenario configuration availability independent of missing token axes', async () => {
    const source = new SearchUsageScenarioSource()
    const page = await source.usageCalls({ filters: {}, order: 'newest', maxItems: 100 })
    expect(
      page.calls.some((call) => call.tokens.output === null && call.cost.status === 'derived'),
    ).toBe(true)
    expect(
      page.calls.some((call) => call.tokens.output !== null && call.cost.status === 'unavailable'),
    ).toBe(true)
    const availability = new Map<string, string>()
    for (const call of page.calls) {
      expect(call.profile_id).toMatch(/^(exact|mapped):.+/)
      const key = `${call.model_id}:${call.profile_id}`
      expect(availability.get(key) ?? call.cost.status).toBe(call.cost.status)
      availability.set(key, call.cost.status)
    }
    for (const group of (await source.usageSummary({})).groups) {
      expect(availability.get(`${group.model_id}:${group.profile_id}`)).toBe(group.cost.status)
    }
  })
})
