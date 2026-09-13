import type { WebUsageCost } from '../generated/web-contract.mjs'
import { enumLabel } from '../labels'

export interface CostTotal {
  amountUsd: string
  unpricedModels: string[]
  incomplete: boolean
  rates: string[]
  breakdown: string[]
}

export function totalCost(
  rows: readonly { cost: WebUsageCost; model_id: string; provenance: string }[],
  incomplete = false,
): CostTotal {
  const amounts = rows.flatMap(({ cost }) => (cost.status === 'derived' ? [cost.amount_usd] : []))
  const scale = Math.max(0, ...amounts.map((amount) => amount.split('.')[1]?.length ?? 0))
  const sum = amounts
    .reduce((total, amount) => {
      const [whole, fraction = ''] = amount.split('.')
      return total + BigInt(`${whole}${fraction.padEnd(scale, '0')}`)
    }, 0n)
    .toString()
    .padStart(scale + 1, '0')
  const rateRows = new Map<string, (typeof rows)[number][]>()
  for (const row of rows) {
    if (row.cost.status !== 'derived') continue
    const key = `${enumLabel(row.provenance)} · ${enumLabel(row.cost.label)} · ${row.cost.rate_version}`
    const group = rateRows.get(key) ?? []
    group.push(row)
    rateRows.set(key, group)
  }
  return {
    breakdown:
      rateRows.size > 1
        ? [...rateRows].map(([key, group]) => `$${totalCost(group).amountUsd} (${key})`)
        : [],
    amountUsd: scale ? `${sum.slice(0, -scale)}.${sum.slice(-scale)}` : sum,
    unpricedModels: [
      ...new Set(
        rows.filter(({ cost }) => cost.status === 'unavailable').map((row) => row.model_id),
      ),
    ],
    incomplete,
    rates: [...rateRows.keys()],
  }
}

export function costTotalText(total: CostTotal): string {
  const priced = total.rates.length > 0 || total.unpricedModels.length === 0
  return [
    total.breakdown.length
      ? `${total.breakdown.join(' + ')}${total.incomplete || total.unpricedModels.length ? ' (partial)' : ''}`
      : priced
        ? `$${total.amountUsd}${total.incomplete || total.unpricedModels.length ? ' (partial)' : ''}`
        : '',
    ...total.unpricedModels.map((model) => `unpriced · ${model}`),
    total.incomplete ? 'incomplete' : '',
  ]
    .filter(Boolean)
    .join(' · ')
}
