import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { expect, it } from 'vitest'
import { SearchUsageScenarioSource } from './scenario'
import { CostChip, turnCosts } from './session-cost'

it('marks turn subtotals incomplete when older calls remain', async () => {
  const source = new SearchUsageScenarioSource()
  const page = await source.usageCalls({ filters: {}, order: 'newest', maxItems: 1 })
  const totals = turnCosts(page)
  expect(totals.size).toBe(1)
  expect([...totals.values()][0]?.incomplete).toBe(true)
})

it('groups complete call evidence by turn without mixing adjacent turns', async () => {
  const source = new SearchUsageScenarioSource()
  const page = await source.usageCalls({ filters: {}, order: 'newest', maxItems: 100 })
  const turnId = page.calls[2]?.turn_id
  if (!turnId) throw new Error('Fixture must have a turn')
  const turnPage = await source.usageCalls({ filters: { turnId }, order: 'newest', maxItems: 100 })
  expect(turnPage.calls).toHaveLength(2)
  const totals = turnCosts(turnPage)
  expect(totals.size).toBe(1)
  expect(totals.get(turnId)?.incomplete).toBe(false)
  expect(totals.get(turnId)?.unpricedModels).toEqual([])
})

it('shows missing cost evidence without linking to an unavailable route or displaying zero', () => {
  const markup = renderToStaticMarkup(createElement(CostChip, { status: 'success' }))
  expect(markup).toContain('Cost not loaded')
  expect(markup).not.toContain('href=')
  expect(markup).not.toContain('$0')
})
