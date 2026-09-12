import { expect, test } from '../../e2e/fontTest'
import { transcriptFixture, transcriptSessionId } from './transcript.fixture'

test('scrolls a hundred thousand messages in both directions with bounded rows', async ({
  page,
}, testInfo) => {
  const reads: string[] = []
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    if (url.pathname.endsWith('/timeline')) reads.push(url.searchParams.get('anchor') ?? '')
    return route.fulfill({ json: transcriptFixture(url) })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByText('Message 100000', { exact: true })).toBeVisible()
  await transcript.hover()
  for (let index = 0; index < 5; index++) {
    await transcript.evaluate((element) => {
      element.scrollTop = 0
    })
    await page.mouse.wheel(0, -900)
    await expect
      .poll(() => reads.filter((anchor) => anchor === 'before').length)
      .toBeGreaterThan(index)
  }
  expect(Number(await transcript.getAttribute('data-total-loaded'))).toBeLessThanOrEqual(24)
  expect(Number(await transcript.getAttribute('data-mounted-rows'))).toBeLessThanOrEqual(24)
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  await page.mouse.wheel(0, 900)
  await expect.poll(() => reads.filter((anchor) => anchor === 'after').length).toBeGreaterThan(0)
  await expect(page.getByRole('button', { name: 'Next text page', exact: true })).toHaveCount(0)
  await page.screenshot({ path: testInfo.outputPath('transcript-scroll.png') })
})

test('automatically crosses a tail containing only lifecycle events', async ({ page }) => {
  const reads: string[] = []
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    const payload = transcriptFixture(url)
    if (url.pathname.endsWith('/timeline')) {
      reads.push(url.searchParams.get('anchor') ?? '')
      const window = payload as import('../generated/web-contract.mjs').WebSessionTimelineWindow
      return route.fulfill({
        json: {
          ...window,
          items: window.items.map((item) =>
            Number(item.address.event_sequence) > 99984
              ? { ...item, kind: 'turn_completed' }
              : item,
          ),
        },
      })
    }
    if (
      url.pathname.endsWith('/timeline-detail') &&
      Number(url.searchParams.get('first')) > 99984
    ) {
      const page = payload as import('../generated/web-contract.mjs').WebSessionTimelineDetailPage
      return route.fulfill({
        json: {
          ...page,
          projected_body_bytes: 128,
          items: page.items.map((item) => ({
            ...item,
            kind: 'turn_completed',
            projected_body_bytes: 128,
            body: {
              type: 'turn_lifecycle',
              turn_id: transcriptSessionId,
              lifecycle: 'terminalized',
              cause_code: 'completed',
            },
          })),
        },
      })
    }
    return route.fulfill({ json: payload })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  await expect(
    page
      .getByRole('region', { name: 'Session transcript', exact: true })
      .getByText('Message 99984', { exact: true }),
  ).toBeVisible()
  expect(reads.filter((anchor) => anchor === 'before').length).toBeGreaterThanOrEqual(2)
})

test('opens the next excerpt directly and closes its expanded text', async ({ page }) => {
  const offsets: string[] = []
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    const payload = transcriptFixture(url)
    if (url.pathname.endsWith('/timeline-detail') && url.searchParams.get('first') === '100000') {
      const detail = payload as import('../generated/web-contract.mjs').WebSessionTimelineDetailPage
      const offset = url.searchParams.get('cursor_offset') ?? '0'
      offsets.push(offset)
      const continuation =
        offset === '0'
          ? {
              address: { event_sequence: '100000' },
              field: 'input_text',
              member_index: 0,
              offset_bytes: '1',
            }
          : null
      return route.fulfill({
        json: {
          ...detail,
          projected_body_bytes: 129,
          continuation: continuation ? { type: 'more_body', body: continuation } : null,
          items: detail.items.map((item) => ({
            ...item,
            projected_body_bytes: 129,
            body: {
              ...item.body,
              text: {
                text: offset === '0' ? 'a' : 'b',
                offset_bytes: offset,
                total_bytes: '2',
                continuation,
              },
            },
          })),
        },
      })
    }
    return route.fulfill({ json: payload })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
  const continuation = transcript.getByRole('region', { name: 'More message text' })
  await expect(continuation.getByText('b', { exact: true })).toBeVisible()
  expect(offsets).toEqual(['0', '1'])
  await continuation.getByRole('button', { name: 'Close details' }).click()
  await expect(continuation).toHaveCount(0)
})
