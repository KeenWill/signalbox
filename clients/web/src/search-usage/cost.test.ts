import { describe, expect, it } from 'vitest'
import type { WebUsageCost } from '../generated/web-contract.mjs'
import { costTotalText, totalCost } from './cost'

const model = 'fixture-model'
const priced = (amount_usd: string) => ({
  model_id: model,
  provenance: 'reported',
  cost: {
    status: 'derived',
    amount_usd,
    label: 'real',
    rate_version: 'fixture-rate',
  } as WebUsageCost,
})

describe('usage totals', () => {
  it('adds fixed point dollars without losing tiny charges', () => {
    expect(
      totalCost([priced('0.1'), priced('0.2'), priced('0.0000000000000000000000000001')]).amountUsd,
    ).toBe('0.3000000000000000000000000001')
  })
  it('names unpriced models and marks priced amounts as partial', () => {
    const text = costTotalText(
      totalCost([
        priced('1.25'),
        {
          model_id: 'unconfigured-model',
          provenance: 'reported',
          cost: { status: 'unavailable', reason: 'configuration_unavailable' },
        },
      ]),
    )
    expect(text).toBe('$1.25 (partial) · unpriced · unconfigured-model')
  })
  it('does not display an unpriced collection as zero dollars', () => {
    expect(
      costTotalText(
        totalCost([
          {
            model_id: model,
            provenance: 'estimated',
            cost: { status: 'unavailable', reason: 'configuration_unavailable' },
          },
        ]),
      ),
    ).toBe(`unpriced · ${model}`)
  })
  it('does not call truncated evidence a complete total', () => {
    expect(costTotalText(totalCost([priced('2')], true))).toBe('$2 (partial) · incomplete')
  })
  it('separates rate and provenance subtotals', () => {
    const total = totalCost([priced('1'), { ...priced('2'), provenance: 'estimated' }])
    expect(total.breakdown).toHaveLength(2)
    expect(costTotalText(total)).toContain('$1 (Reported')
    expect(costTotalText(total)).toContain('$2 (Estimated')
  })
})
