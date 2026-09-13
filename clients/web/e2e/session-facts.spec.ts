import type { Request } from '@playwright/test'
import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'
import { expect, test } from './fontTest'
import {
  openSession,
  sessionApi,
  sessionId,
  turnId,
  type WebRepositoryWatchProvenance,
} from './session-fixture'

const origin = {
  dispatch_id: turnId,
  action_ordinal: '1',
  repository: 'signalbox/example',
  pull_request: '81',
  rule_id: 'review-response',
  rule_revision: '3',
  event_id: sessionId,
  event_kind: 'review_submitted',
} as WebRepositoryWatchProvenance
const group = (amount: string, label = 'real') => ({
  call_kind: 'model_call',
  model_id: turnId,
  profile_id: `exact:test-${amount}`,
  provenance: 'reported',
  input_semantics: 'cache_exclusive',
  coverage: { input: true, output: true, cache_creation_input: false, cache_read_input: false },
  call_count: '1',
  tokens: { input: '10', output: '10', cache_creation_input: null, cache_read_input: null },
  cost: { status: 'derived', amount_usd: amount, rate_version: 'test', label },
})
for (const viewport of [
  { name: 'desktop', width: 1440, height: 1000 },
  { name: 'phone', width: 390, height: 844 },
]) {
  test(`session links and cost ${viewport.name}`, async ({ page }, testInfo) => {
    await page.setViewportSize(viewport)
    const api = await sessionApi(page, false, sessionId, origin)
    const reads: string[] = []
    await page.route('**/api/usage/summary?**', (route) => {
      reads.push(new URL(route.request().url()).searchParams.get('session_id')!)
      return route.fulfill({
        json: { groups: [group(api.state.grown ? '3.2' : '1.2'), group('0.3')], truncated: false },
      })
    })
    await openSession(page)
    await expect(page.getByTitle('Session cost', { exact: true })).toHaveText('$1.50')
    await expect(
      page.getByRole('link', { name: 'signalbox/example', exact: true }),
    ).toHaveAttribute('href', 'https://github.com/signalbox/example')
    await expect(page.getByRole('link', { name: '#81', exact: true })).toHaveAttribute(
      'href',
      'https://github.com/signalbox/example/pull/81',
    )
    await expect(page.getByTitle('Rule review-response · Review submitted')).toBeVisible()
    if (viewport.name === 'desktop')
      expect(
        (await page.locator('.session-compact-header').boundingBox())?.height,
      ).toBeLessThanOrEqual(70)
    const header = await page.locator('.session-compact-header').boundingBox()
    expect(header!.x + header!.width).toBeLessThanOrEqual(viewport.width)
    await expect(page.getByRole('button', { name: 'Latest', exact: true })).toBeInViewport({
      ratio: 1,
    })
    await page.screenshot({
      path: testInfo.outputPath(`facts-${viewport.name}.png`),
      fullPage: true,
    })
    api.grow()
    await expect(page.getByTitle('Session cost', { exact: true })).toHaveText('$3.50')
    expect(reads.every((id) => id === sessionId)).toBe(true)
  })
}
for (const example of [
  {
    name: 'decimal rounding',
    groups: [group('0.001'), group('1.134')],
    truncated: false,
    text: '$1.14',
  },
  {
    name: 'large dollars',
    groups: [group('9007199254740993.01')],
    truncated: false,
    text: '$9,007,199,254,740,993.01',
  },
  { name: 'empty', groups: [], truncated: false, text: '$0.00' },
  { name: 'truncated', groups: [group('2')], truncated: true, text: 'Cost incomplete' },
  {
    name: 'unpriced',
    groups: [
      group('2'),
      { ...group('1'), cost: { status: 'unavailable', reason: 'configuration_unavailable' } },
    ],
    truncated: false,
    text: 'Cost unavailable',
  },
  {
    name: 'equivalent',
    groups: [group('2'), group('1', 'metered_equivalent')],
    truncated: false,
    text: '$3.00 equivalent',
  },
])
  test(`cost handles ${example.name}`, async ({ page }) => {
    await sessionApi(page)
    await page.route('**/api/usage/summary?**', (route) =>
      route.fulfill({ json: { groups: example.groups, truncated: example.truncated } }),
    )
    await openSession(page)
    await expect(page.getByTitle('Session cost', { exact: true })).toHaveText(example.text)
  })

test('leaving a session cancels its pending cost bootstrap', async ({ page }) => {
  const api = await sessionApi(page)
  await page.route('**/api/usage/summary?**', (route) =>
    route.fulfill({ json: { groups: [], truncated: false } }),
  )
  await openSession(page)
  await expect(page.getByTitle('Session cost', { exact: true })).toHaveText('$0.00')
  let blocked: Request | null = null
  let release = () => {}
  const waiting = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/api/bootstrap', async (route) => {
    blocked = route.request()
    await waiting
    await route.fulfill({ json: bootstrapFixture })
  })
  api.grow()
  await expect.poll(() => blocked !== null).toBe(true)
  await page.getByRole('link', { name: 'Settings', exact: true }).click()
  await expect.poll(() => blocked?.failure()?.errorText).toBeTruthy()
  release()
})

test('wide costs keep the phone header and timeline controls in view', async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await sessionApi(page)
  await page.route('**/api/usage/summary?**', (route) =>
    route.fulfill({
      json: { groups: [group('12345678901234567890123456789')], truncated: false },
    }),
  )
  await openSession(page)
  const cost = page.getByTitle('Session cost', { exact: true })
  await expect(cost).toHaveText('$12,345,678,901,234,567,890,123,456,789.00')
  await expect(cost).toBeInViewport({ ratio: 1 })
  await expect(page.getByRole('heading', { name: 'Session', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'First', exact: true })).toBeInViewport({
    ratio: 1,
  })
  await expect(page.getByRole('button', { name: 'Latest', exact: true })).toBeInViewport({
    ratio: 1,
  })
  const header = await page.locator('.session-compact-header').boundingBox()
  expect(header!.x + header!.width).toBeLessThanOrEqual(390)
  await page.screenshot({ path: testInfo.outputPath('facts-wide-cost-phone.png'), fullPage: true })
})

test('cost refresh completes during session growth and catches up without hiding its total', async ({
  page,
}) => {
  const api = await sessionApi(page)
  const requests: Request[] = []
  let releaseFirst = () => {}
  let releaseSecond = () => {}
  const first = new Promise<void>((resolve) => {
    releaseFirst = resolve
  })
  const second = new Promise<void>((resolve) => {
    releaseSecond = resolve
  })
  await page.route('**/api/usage/summary?**', async (route) => {
    requests.push(route.request())
    const initial = requests.length === 1
    await (initial ? first : second)
    await route.fulfill({ json: { groups: [group(initial ? '1.5' : '3.5')], truncated: false } })
  })
  await openSession(page)
  await expect.poll(() => requests.length).toBe(1)
  api.grow()
  await page.getByText('Session details', { exact: true }).click()
  await expect(page.locator('.session-header-detail-content')).toContainText('44')
  releaseFirst()
  const cost = page.getByTitle('Session cost', { exact: true })
  await expect(cost).toHaveText('$1.50')
  await expect.poll(() => requests.length).toBe(2)
  expect(requests[0]!.failure()).toBeNull()
  releaseSecond()
  await expect(cost).toHaveText('$3.50')
  expect(requests).toHaveLength(2)
})
