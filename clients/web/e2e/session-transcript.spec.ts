import type {
  WebSessionTimelineDetailBody,
  WebTimelineBodyField,
  WebTimelineTextExcerpt,
} from '../src/generated/web-contract.mjs'
import { transcriptFixture, transcriptSessionId } from '../src/session-timeline/transcript.fixture'
import { expect, test } from './fontTest'
import { detailItems, detailPage, resultCursor, toolResultItem } from './session-detail-fixture'

test('rejects contradictory detail kinds and recovers after a corrected retry', async ({
  page,
}) => {
  let conflict = true
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    if (conflict && url.pathname.endsWith('/timeline-detail'))
      return route.fulfill({
        json: {
          session_id: transcriptSessionId,
          projected_body_bytes: 128,
          continuation: null,
          items: [
            {
              address: { event_sequence: '1' },
              kind: 'turn_completed',
              projected_body_bytes: 128,
              body: {
                type: 'turn_lifecycle',
                turn_id: transcriptSessionId,
                lifecycle: 'terminalized',
                cause_code: 'completed',
              },
            },
          ],
        },
      })
    return route.fulfill({ json: transcriptFixture(url, 1) })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
  await expect(surface.getByRole('alert')).toContainText('Transcript failed to load.')
  await expect(surface.getByText('No messages', { exact: false })).toHaveCount(0)
  await expect(surface.getByText('Message 1', { exact: true })).toHaveCount(0)
  conflict = false
  await surface.getByRole('button', { name: 'Retry transcript' }).click()
  await expect(surface.getByText('Message 1', { exact: true })).toBeVisible()
  await expect(surface.getByRole('alert')).toHaveCount(0)
})

test('rejects a continued detail kind that contradicts the header of an empty initial page', async ({
  page,
}) => {
  let conflict = true
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
      if (!url.searchParams.has('cursor_address'))
        return route.fulfill({
          json: {
            session_id: transcriptSessionId,
            items: [],
            projected_body_bytes: 0,
            continuation: { type: 'more_at', address: { event_sequence: '100000' } },
          },
        })
      if (conflict)
        return route.fulfill({
          json: {
            session_id: transcriptSessionId,
            projected_body_bytes: 128,
            continuation: null,
            items: [
              {
                address: { event_sequence: '100000' },
                kind: 'turn_completed',
                projected_body_bytes: 128,
                body: {
                  type: 'turn_lifecycle',
                  turn_id: transcriptSessionId,
                  lifecycle: 'terminalized',
                  cause_code: 'completed',
                },
              },
            ],
          },
        })
    }
    return route.fulfill({ json: payload })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
  const detail = transcript.getByRole('region', { name: 'More message text', exact: true })
  await expect(detail.getByRole('alert')).toContainText('Details could not be loaded.')
  await expect(detail.getByText('Message 100000', { exact: true })).toHaveCount(0)
  conflict = false
  await detail.getByRole('button', { name: 'Retry details', exact: true }).click()
  await expect(detail.getByText('Message 100000', { exact: true })).toBeVisible()
  await expect(detail.getByRole('alert')).toHaveCount(0)
})

for (const direction of ['before', 'after'] as const) {
  test(`retries the failed ${direction} edge without refreshing retained windows`, async ({
    page,
  }) => {
    const reads: string[] = []
    let attempts = 0
    await page.route('**/api/**', (route) => {
      const url = new URL(route.request().url())
      if (url.pathname.endsWith('/follow'))
        return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
      if (url.pathname === '/api/attention')
        return route.fulfill({
          json: { cursor: '0', summaries: [], continuation_after_session_id: null },
        })
      if (url.pathname.endsWith('/timeline')) {
        const anchor = url.searchParams.get('anchor') ?? ''
        reads.push(`${anchor}:${url.searchParams.get('address') ?? ''}`)
        if (anchor === direction && attempts++ === 0) return route.abort('failed')
      }
      return route.fulfill({ json: transcriptFixture(url, 24) })
    })
    await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
    await expect(transcript.getByText('Message 24', { exact: true })).toBeVisible()
    if (direction === 'after') {
      await page.getByRole('button', { name: 'First', exact: true }).click()
      await expect(transcript.getByText('Message 1', { exact: true })).toBeVisible()
    }
    await expect(surface).toHaveAttribute('aria-busy', 'false')
    await transcript.evaluate((element, direction) => {
      element.scrollTop = direction === 'before' ? 0 : element.scrollHeight
      element.dispatchEvent(
        new WheelEvent('wheel', {
          deltaY: direction === 'before' ? -900 : 900,
          bubbles: true,
        }),
      )
    }, direction)
    await expect(surface.getByRole('alert')).toContainText('Transcript failed to load.')
    expect(attempts).toBe(1)
    const failed = reads.at(-1)
    const beforeRetry = reads.length
    await surface.getByRole('button', { name: 'Retry transcript', exact: true }).click()
    await expect(transcript).toHaveAttribute('data-total-loaded', '16')
    await expect(surface).toHaveAttribute('aria-busy', 'false')
    await expect(surface.getByRole('alert')).toHaveCount(0)
    expect(attempts).toBe(2)
    expect(reads.slice(beforeRetry)).toEqual([failed])
    await expect(transcript.locator('[data-event-sequence="12"]')).toHaveCount(1)
  })
}

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
  const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
  for (let index = 0; index < 5; index++) {
    await expect(surface).toHaveAttribute('aria-busy', 'false')
    const previousReads = reads.filter((anchor) => anchor === 'before').length
    await transcript.evaluate((element) => {
      element.scrollTop = 0
    })
    await page.mouse.wheel(0, -900)
    await expect
      .poll(() => reads.filter((anchor) => anchor === 'before').length)
      .toBeGreaterThan(previousReads)
    await expect(surface).toHaveAttribute('aria-busy', 'false')
  }
  expect(Number(await transcript.getAttribute('data-total-loaded'))).toBeLessThanOrEqual(24)
  expect(Number(await transcript.getAttribute('data-mounted-rows'))).toBeLessThanOrEqual(24)
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
    element.dispatchEvent(new WheelEvent('wheel', { deltaY: 900, bubbles: true }))
  })
  await expect.poll(() => reads.filter((anchor) => anchor === 'after').length).toBeGreaterThan(0)
  await expect(surface).toHaveAttribute('aria-busy', 'false')
  // Allow completed-query renders and their scroll adjustments to settle.
  await page.waitForTimeout(500)
  expect(reads.filter((anchor) => anchor === 'after')).toHaveLength(1)
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
    element.dispatchEvent(new WheelEvent('wheel', { deltaY: 900, bubbles: true }))
  })
  await expect.poll(() => reads.filter((anchor) => anchor === 'after').length).toBe(2)
  await expect(page.getByRole('button', { name: 'Next text page', exact: true })).toHaveCount(0)
  await page.screenshot({ path: testInfo.outputPath('transcript-scroll.png') })
})

test('preserves the reading position when loading the final later window', async ({
  page,
}, testInfo) => {
  const problems: string[] = []
  page.on('pageerror', (error) => problems.push(error.message))
  page.on('console', (message) => {
    if (message.type() === 'error') problems.push(message.text())
  })
  let latest = 16
  let releaseGrowth = () => {}
  const growth = new Promise<void>((resolve) => {
    releaseGrowth = resolve
  })
  let requestedLater = false
  let releaseLater = () => {}
  const later = new Promise<void>((resolve) => {
    releaseLater = resolve
  })
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname === `/api/sessions/${transcriptSessionId}/follow`) {
      await growth
      return route.fulfill({
        contentType: 'application/x-ndjson',
        body:
          [
            {
              kind: 'snapshot',
              snapshot: {
                session_id: transcriptSessionId,
                observed_through: '16',
                active: null,
                queued_turn_count: '0',
                queued_turn_ids: [],
                reconciliation: null,
                runner: null,
              },
            },
            {
              kind: 'durable',
              cursor: '17',
              address: { event_sequence: '17' },
              event_kind: 'input_accepted',
            },
          ]
            .map((event) => JSON.stringify(event))
            .join('\n') + '\n',
      })
    }
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    if (url.pathname.endsWith('/timeline') && url.searchParams.get('anchor') === 'after') {
      requestedLater = true
      await later
    }
    return route.fulfill({ json: transcriptFixture(url, latest) })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
  await expect(transcript.getByText('Message 16', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: /^First/ }).click()
  await expect(transcript.getByText('Message 1', { exact: true })).toBeVisible()
  await expect(surface).toHaveAttribute('aria-busy', 'false')
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  await expect.poll(() => requestedLater).toBe(true)
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  const anchor = transcript.getByText('Message 8', { exact: true })
  await expect(anchor).toBeVisible()
  const anchorTop = await anchor.evaluate((element) => element.getBoundingClientRect().top)
  releaseLater()
  await expect(surface).toHaveAttribute('aria-busy', 'false')
  await expect(transcript).toHaveAttribute('data-total-loaded', '16')
  await expect
    .poll(async () =>
      Math.abs(
        (await anchor.evaluate((element) => element.getBoundingClientRect().top)) - anchorTop,
      ),
    )
    .toBeLessThan(2)
  await expect(transcript.getByText('Message 16', { exact: true })).not.toBeInViewport()
  await page.screenshot({ path: testInfo.outputPath('final-window-anchor.png') })
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  await expect(transcript.getByText('Message 16', { exact: true })).toBeVisible()
  latest = 17
  releaseGrowth()
  await expect(transcript.getByText('Message 17', { exact: true })).toBeVisible()
  await expect
    .poll(() =>
      transcript.evaluate(
        (element) => element.scrollHeight - element.scrollTop - element.clientHeight,
      ),
    )
    .toBeLessThanOrEqual(1)
  expect(problems).toEqual([])
})

for (const [maximumItems, internal] of [
  [1, false],
  [8, false],
  [8, true],
] as const) {
  test(`automatically crosses a lifecycle-only ${internal ? 'internal window' : 'tail'} with ${maximumItems} items per page`, async ({
    page,
  }) => {
    const reads: string[] = []
    const hidden = (sequence: string | null) =>
      Number(sequence) > 99984 && (!internal || Number(sequence) <= 99992)
    await page.route('**/api/**', (route) => {
      const url = new URL(route.request().url())
      if (url.pathname.endsWith('/follow'))
        return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
      if (url.pathname === '/api/attention')
        return route.fulfill({
          json: { cursor: '0', summaries: [], continuation_after_session_id: null },
        })
      const payload = transcriptFixture(url)
      if (url.pathname === '/api/bootstrap') {
        const bootstrap =
          payload as typeof import('../src/product.fixture').webContractBootstrapFixture
        return route.fulfill({
          json: {
            ...bootstrap,
            limits: { ...bootstrap.limits, max_timeline_window_items: maximumItems },
          },
        })
      }
      if (url.pathname.endsWith('/timeline')) {
        reads.push(url.searchParams.get('anchor') ?? '')
        const window =
          payload as import('../src/generated/web-contract.mjs').WebSessionTimelineWindow
        return route.fulfill({
          json: {
            ...window,
            items: window.items.map((item) =>
              hidden(item.address.event_sequence) ? { ...item, kind: 'turn_completed' } : item,
            ),
          },
        })
      }
      if (url.pathname.endsWith('/timeline-detail') && hidden(url.searchParams.get('first'))) {
        const page =
          payload as import('../src/generated/web-contract.mjs').WebSessionTimelineDetailPage
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
    if (internal) {
      const transcript = page.getByRole('region', {
        name: 'Session transcript',
        exact: true,
      })
      await expect(transcript.getByText('Message 100000', { exact: true })).toBeVisible()
      await transcript.hover()
      await transcript.evaluate((element) => {
        element.scrollTop = 0
      })
      await page.mouse.wheel(0, -900)
    }
    await expect(
      page
        .getByRole('region', { name: 'Session transcript', exact: true })
        .getByText('Message 99984', { exact: true }),
    ).toBeVisible()
    expect(reads.filter((anchor) => anchor === 'before').length).toBeGreaterThanOrEqual(2)
  })
}

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
      const detail =
        payload as import('../src/generated/web-contract.mjs').WebSessionTimelineDetailPage
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
  const continuation = transcript.getByRole('region', {
    name: 'More message text',
  })
  await expect(continuation.getByText('b', { exact: true })).toBeVisible()
  expect(offsets).toEqual(['0', '1'])
  await continuation.getByRole('button', { name: 'Close details' }).click()
  await expect(continuation).toHaveCount(0)
})

test('retains the continued reader through virtual unmounts and keeps focused controls mounted', async ({
  page,
}) => {
  const offsets: string[] = []
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    const payload = transcriptFixture(url, 24)
    if (url.pathname.endsWith('/timeline-detail') && url.searchParams.get('first') === '24') {
      const detail =
        payload as import('../src/generated/web-contract.mjs').WebSessionTimelineDetailPage
      const offset = url.searchParams.get('cursor_offset') ?? '0'
      offsets.push(offset)
      const continuation =
        offset === '3'
          ? null
          : {
              address: { event_sequence: '24' },
              field: 'input_text',
              member_index: 0,
              offset_bytes: String(Number(offset) + 1),
            }
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
                text: 'abcd'[Number(offset)],
                offset_bytes: offset,
                total_bytes: '4',
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
  await page.addStyleTag({ content: '.session-message-entry { min-height: 200px; }' })
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
  await expect(transcript.getByRole('button', { name: 'Read more', exact: true })).toBeVisible()
  for (const loaded of [16, 24]) {
    await expect(surface).toHaveAttribute('aria-busy', 'false')
    await transcript.evaluate((element) => {
      element.scrollTop = 0
      element.dispatchEvent(new WheelEvent('wheel', { deltaY: -900, bubbles: true }))
    })
    await expect(transcript).toHaveAttribute('data-total-loaded', String(loaded))
  }
  await expect(surface).toHaveAttribute('aria-busy', 'false')
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  const row = transcript.locator('[data-event-sequence="24"]')
  await row.getByRole('button', { name: 'Read more', exact: true }).click()
  await expect(row.getByText('b', { exact: true })).toBeVisible()
  await row.getByRole('button', { name: 'Continue reading', exact: true }).click()
  await expect(row.getByText('c', { exact: true })).toBeVisible()
  const advance = row.getByRole('button', { name: 'Continue reading', exact: true })
  await advance.focus()
  await transcript.evaluate((element) => {
    element.scrollTop = 0
  })
  await expect(advance).toBeFocused()
  await expect(row).toHaveCount(1)
  await transcript.focus()
  await expect(row).toHaveCount(0)
  expect(Number(await transcript.getAttribute('data-mounted-rows'))).toBeLessThanOrEqual(24)
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  await expect(row.getByText('c', { exact: true })).toBeVisible()
  expect(offsets).toEqual(['0', '1', '2'])
  await advance.click()
  await expect(row.getByText('d', { exact: true })).toBeVisible()
  expect(offsets).toEqual(['0', '1', '2', '3'])
  await row.getByRole('button', { name: 'Close details', exact: true }).click()
  const opener = row.getByRole('button', { name: 'Read more', exact: true })
  await expect(opener).toBeFocused()
  await opener.click()
  await expect(row.getByText('b', { exact: true })).toBeVisible()
  expect(offsets).toEqual(['0', '1', '2', '3', '1'])
})

for (const mode of ['earlier', 'hidden', 'historical']) {
  const hiddenTail = mode === 'hidden'
  test(
    mode === 'historical'
      ? 'refreshes retained history after the actual tail has been evicted'
      : `follows live growth after ${hiddenTail ? 'automatically scanning a hidden tail' : 'loading earlier history and returning to the end'}`,
    async ({ page }) => {
      const problems: string[] = []
      page.on('pageerror', (error) => problems.push(error.message))
      page.on('console', (message) => {
        if (message.type() === 'error') problems.push(message.text())
      })
      let latest = 100000
      let release = () => {}
      const growth = new Promise<void>((resolve) => {
        release = resolve
      })
      const anchors: string[] = []
      let releaseLater = () => {}
      const later = new Promise<void>((resolve) => {
        releaseLater = resolve
      })
      await page.route('**/api/**', async (route) => {
        const url = new URL(route.request().url())
        if (url.pathname === `/api/sessions/${transcriptSessionId}/follow`) {
          await growth
          return route.fulfill({
            contentType: 'application/x-ndjson',
            body:
              [
                {
                  kind: 'snapshot',
                  snapshot: {
                    session_id: transcriptSessionId,
                    observed_through: '100000',
                    active: null,
                    queued_turn_count: '0',
                    queued_turn_ids: [],
                    reconciliation: null,
                    runner: null,
                  },
                },
                {
                  kind: 'durable',
                  cursor: '100001',
                  address: { event_sequence: '100001' },
                  event_kind: 'input_accepted',
                },
              ]
                .map((event) => JSON.stringify(event))
                .join('\n') + '\n',
          })
        }
        if (url.pathname.endsWith('/follow'))
          return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
        if (url.pathname === '/api/attention')
          return route.fulfill({
            json: { cursor: '0', summaries: [], continuation_after_session_id: null },
          })
        if (url.pathname.endsWith('/timeline')) anchors.push(url.searchParams.get('anchor') ?? '')
        if (
          mode === 'historical' &&
          url.pathname.endsWith('/timeline') &&
          url.searchParams.get('anchor') === 'after'
        )
          await later
        const payload = transcriptFixture(url, latest)
        const hidden = (sequence: string) =>
          hiddenTail && Number(sequence) > 99968 && Number(sequence) <= 100000
        if (url.pathname.endsWith('/timeline')) {
          const window =
            payload as import('../src/generated/web-contract.mjs').WebSessionTimelineWindow
          return route.fulfill({
            json: {
              ...window,
              items: window.items.map((item) =>
                hidden(item.address.event_sequence) ? { ...item, kind: 'turn_completed' } : item,
              ),
            },
          })
        }
        if (
          url.pathname.endsWith('/timeline-detail') &&
          hidden(url.searchParams.get('first') ?? '0')
        ) {
          const detail =
            payload as import('../src/generated/web-contract.mjs').WebSessionTimelineDetailPage
          return route.fulfill({
            json: {
              ...detail,
              projected_body_bytes: 128,
              items: detail.items.map((item) => ({
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
      const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
      if (hiddenTail) {
        await expect(transcript.getByText('Message 99968', { exact: true })).toBeVisible()
      } else if (mode === 'historical') {
        await expect(transcript.getByText('Message 100000', { exact: true })).toBeVisible()
        const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
        await transcript.hover()
        for (let index = 0; index < 5; index++) {
          await expect(surface).toHaveAttribute('aria-busy', 'false')
          const previousReads = anchors.filter((anchor) => anchor === 'before').length
          await transcript.evaluate((element) => {
            element.scrollTop = 0
          })
          await page.mouse.wheel(0, -900)
          await expect
            .poll(() => anchors.filter((anchor) => anchor === 'before').length)
            .toBeGreaterThan(previousReads)
        }
        await expect(surface).toHaveAttribute('aria-busy', 'false')
        await transcript.evaluate((element) => {
          element.scrollTop = element.scrollHeight
        })
        await expect
          .poll(() => anchors.filter((anchor) => anchor === 'after').length)
          .toBeGreaterThan(0)
      } else {
        await expect(transcript.getByText('Message 100000', { exact: true })).toBeVisible()
        await transcript.evaluate((element) => {
          element.scrollTop = 0
        })
        await transcript.hover()
        await page.mouse.wheel(0, -900)
        await expect.poll(() => anchors).toContain('before')
        await expect
          .poll(async () => Number(await transcript.getAttribute('data-total-loaded')))
          .toBeGreaterThanOrEqual(16)
        await expect(
          page.getByRole('region', { name: 'Transcript text', exact: true }),
        ).toHaveAttribute('aria-busy', 'false')
        await transcript.evaluate((element) => {
          element.scrollTop = element.scrollHeight
        })
        await page.mouse.wheel(0, 900)
        await expect(transcript.getByText('Message 100000', { exact: true })).toBeVisible()
      }
      const beforeGrowth = anchors.length
      latest = 100001
      release()
      if (mode === 'historical') {
        await expect.poll(() => anchors.length).toBeGreaterThan(beforeGrowth)
        releaseLater()
        await expect(
          page.getByRole('region', { name: 'Transcript text', exact: true }),
        ).toHaveAttribute('aria-busy', 'false')
        expect(anchors.slice(beforeGrowth)).not.toContain('latest')
        await expect(transcript.getByText('Message 100001', { exact: true })).toHaveCount(0)
        expect(Number(await transcript.getAttribute('data-total-loaded'))).toBeLessThanOrEqual(24)
      } else {
        await expect(transcript.getByText('Message 100001', { exact: true })).toBeVisible()
      }
      expect(problems).toEqual([])
    },
  )
}

test('scans backward after forward paging reaches a newly hidden live tail', async ({ page }) => {
  let latest = 16
  let release = () => {}
  const growth = new Promise<void>((resolve) => {
    release = resolve
  })
  const anchors: string[] = []
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow')) {
      await growth
      return route.fulfill({
        contentType: 'application/x-ndjson',
        body:
          [
            {
              kind: 'snapshot',
              snapshot: {
                session_id: transcriptSessionId,
                observed_through: '16',
                active: null,
                queued_turn_count: '0',
                queued_turn_ids: [],
                reconciliation: null,
                runner: null,
              },
            },
            {
              kind: 'durable',
              cursor: '48',
              address: { event_sequence: '48' },
              event_kind: 'turn_completed',
            },
          ]
            .map((event) => JSON.stringify(event))
            .join('\n') + '\n',
      })
    }
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    const payload = transcriptFixture(url, latest)
    if (url.pathname.endsWith('/timeline')) {
      anchors.push(url.searchParams.get('anchor') ?? '')
      const window = payload as import('../src/generated/web-contract.mjs').WebSessionTimelineWindow
      return route.fulfill({
        json: {
          ...window,
          items: window.items.map((item) =>
            Number(item.address.event_sequence) > 16 ? { ...item, kind: 'turn_completed' } : item,
          ),
        },
      })
    }
    if (url.pathname.endsWith('/timeline-detail') && Number(url.searchParams.get('first')) > 16) {
      const detail =
        payload as import('../src/generated/web-contract.mjs').WebSessionTimelineDetailPage
      return route.fulfill({
        json: {
          ...detail,
          projected_body_bytes: 128,
          items: detail.items.map((item) => ({
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
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
  await expect(transcript.getByText('Message 16', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'First', exact: true }).click()
  await expect(transcript.getByText('Message 1', { exact: true })).toBeVisible()
  await expect(surface).toHaveAttribute('aria-busy', 'false')
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
    element.dispatchEvent(new WheelEvent('wheel', { deltaY: 900, bubbles: true }))
  })
  await expect(transcript).toHaveAttribute('data-total-loaded', '16')
  await expect(surface).toHaveAttribute('aria-busy', 'false')
  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
    element.dispatchEvent(new WheelEvent('wheel', { deltaY: 900, bubbles: true }))
  })
  await expect(transcript.getByText('Message 16', { exact: true })).toBeVisible()
  const beforeGrowth = anchors.length
  latest = 48
  release()
  await expect.poll(() => anchors.slice(beforeGrowth)).toContain('latest')
  await expect.poll(() => anchors.slice(beforeGrowth)).toContain('before')
  await expect(transcript.getByText('Message 16', { exact: true })).toBeVisible()
  await expect(surface).toHaveAttribute('aria-busy', 'false')
  expect(Number(await transcript.getAttribute('data-total-loaded'))).toBeLessThanOrEqual(24)
})

test('keeps an empty detail continuation available without retrying it automatically', async ({
  page,
}) => {
  const cursors: (string | null)[] = []
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    if (url.pathname.endsWith('/timeline-detail') && url.searchParams.get('first') === '100000') {
      cursors.push(url.searchParams.get('cursor_address'))
      if (!url.searchParams.has('cursor_address'))
        return route.fulfill({
          json: {
            session_id: transcriptSessionId,
            items: [],
            projected_body_bytes: 0,
            continuation: {
              type: 'more_at',
              address: { event_sequence: '100000' },
            },
          },
        })
    }
    return route.fulfill({ json: transcriptFixture(url) })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByRole('button', { name: 'Read more', exact: true })).toBeVisible()
  expect(cursors).toEqual([null])
  await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
  await expect(transcript.getByText('Message 100000', { exact: true })).toBeVisible()
  expect(cursors).toEqual([null, '100000'])
})

test('follows workspace navigation and restores its saved transcript anchor', async ({ page }) => {
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    return route.fulfill({ json: transcriptFixture(url) })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByText('Message 100000', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: /^First/ }).click()
  await expect(transcript.getByText('Message 1', { exact: true })).toBeVisible()
  await page.getByText('Session details', { exact: true }).click()
  await page.getByRole('button', { name: 'Next', exact: true }).click()
  await page.keyboard.press('Escape')
  await expect(transcript.getByText('Message 81', { exact: true })).toBeVisible()
  await page.evaluate((sessionId) => {
    const preferences = JSON.parse(localStorage.getItem('signalbox.web.preferences.v1') ?? '{}')
    preferences.lastLogicalPositions = { ...preferences.lastLogicalPositions, [sessionId]: '500' }
    localStorage.setItem('signalbox.web.preferences.v1', JSON.stringify(preferences))
  }, transcriptSessionId)
  await page.reload()
  await expect(transcript.getByText('Message 500', { exact: true })).toBeVisible()
})

test('uses the scrolling transcript as the single conversation focus entry', async ({ page }) => {
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    return route.fulfill({ json: transcriptFixture(url) })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const conversation = page.getByRole('region', { name: 'Conversation', exact: true })
  const transcript = conversation.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByText('Message 100000', { exact: true })).toBeVisible()
  await expect(conversation).not.toHaveAttribute('tabindex', '0')
  await expect(conversation.locator('[tabindex="0"]')).toHaveCount(1)
  const latest = page.getByRole('button', { name: /Latest/ })
  await page.getByRole('button', { name: 'Switch to focus layout', exact: true }).click()
  await expect(transcript).toBeFocused()
  await page.getByRole('button', { name: 'Switch to workbench layout', exact: true }).click()
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await latest.focus()
  await page.keyboard.press('j')
  await expect(page.getByRole('grid', { name: 'Session timeline', exact: true })).toBeFocused()
  await page.getByRole('checkbox', { name: 'Events', exact: true }).uncheck()
  await page.getByRole('button', { name: 'Switch to focus layout', exact: true }).click()
  await expect(transcript).toBeFocused()
})

test('keeps a terminal result reachable when its repeated arguments are hidden', async ({
  page,
}) => {
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    if (url.pathname.endsWith('/timeline')) {
      const kind = 'tool_batch_transition'
      const items = ['1', '2'].map((event_sequence) => ({
        address: { event_sequence },
        kind,
        projected_structured_bytes: 64 + kind.length,
      }))
      return route.fulfill({
        json: {
          session_id: transcriptSessionId,
          items,
          projected_structured_bytes: items.reduce(
            (sum, item) => sum + item.projected_structured_bytes,
            0,
          ),
          continuation_before: null,
          continuation_after: null,
        },
      })
    }
    if (url.pathname.endsWith('/timeline-detail')) {
      const item = detailItems[1]
      if (item?.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
      if (url.searchParams.get('cursor_field') === 'tool_result')
        return route.fulfill({ json: detailPage([toolResultItem()]) })
      if (url.searchParams.get('first') === '1')
        return route.fulfill({
          json: detailPage([
            {
              ...item,
              address: { event_sequence: '1' },
              body: {
                ...item.body,
                tools: item.body.tools.map((tool) => ({
                  ...tool,
                  evidence: { type: 'request_only' },
                })),
              },
            },
          ]),
        })
      return route.fulfill({ json: detailPage([item], resultCursor) })
    }
    return route.fulfill({ json: transcriptFixture(url, 2) })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByRole('region', { name: 'Arguments', exact: true })).toHaveCount(1)
  await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
  await expect(transcript.getByRole('region', { name: 'More message text' })).toContainText(
    'passed',
  )
})

for (const [itemLimit, byteLimit, expectedReads, retry] of [
  [1, 65536, 1, false],
  [10, 65536, 10, false],
  [128, 1024, 7, false],
  [9, 65536, 9, true],
  [9, 65536, 9, 'gesture'],
  [9, 65536, 9, 'retry-gesture'],
  [128, 1024, 7, true],
] as const) {
  test(`charges discarded details to the ${itemLimit}-item / ${byteLimit}-byte automatic scan budget${typeof retry === 'string' ? ` with a fresh ${retry} after failure` : retry ? ' across failed retries' : ''}`, async ({
    page,
  }) => {
    const reads: URL[] = []
    const headers: URL[] = []
    const failed: string[] = []
    let minDetailBytes = 0
    await page.route('**/api/**', (route) => {
      const url = new URL(route.request().url())
      if (url.pathname.endsWith('/follow'))
        return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
      if (url.pathname === '/api/attention')
        return route.fulfill({
          json: { cursor: '0', summaries: [], continuation_after_session_id: null },
        })
      const payload = transcriptFixture(url)
      if (url.pathname === '/api/bootstrap') {
        const bootstrap =
          payload as typeof import('../src/product.fixture').webContractBootstrapFixture
        minDetailBytes = bootstrap.limits.min_timeline_detail_bytes
        return route.fulfill({
          json: {
            ...bootstrap,
            limits: {
              ...bootstrap.limits,
              max_timeline_detail_items: itemLimit,
              max_timeline_detail_bytes: byteLimit,
            },
          },
        })
      }
      if (url.pathname.endsWith('/timeline')) {
        headers.push(url)
        if (
          retry &&
          failed.length < (retry === 'gesture' ? 1 : 2) &&
          url.searchParams.get('anchor') === 'before' &&
          url.searchParams.get('max_items') === '1'
        ) {
          failed.push(url.search)
          return route.abort('failed')
        }
        const window =
          payload as import('../src/generated/web-contract.mjs').WebSessionTimelineWindow
        const kind = 'turn_completed'
        return route.fulfill({
          json: {
            ...window,
            items: window.items.map((item) => ({
              ...item,
              kind,
              projected_structured_bytes: 64 + kind.length,
            })),
            projected_structured_bytes: window.items.length * (64 + kind.length),
          },
        })
      }
      if (url.pathname.endsWith('/timeline-detail')) {
        reads.push(url)
        const detail =
          payload as import('../src/generated/web-contract.mjs').WebSessionTimelineDetailPage
        return route.fulfill({
          json: {
            ...detail,
            projected_body_bytes: 128,
            items: detail.items.map((item) => ({
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
    const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
    if (typeof retry === 'string') {
      await expect(surface.getByRole('alert')).toContainText('Transcript failed to load.')
      expect(reads).toHaveLength(expectedReads - 1)
      if (retry === 'retry-gesture') {
        await surface.getByRole('button', { name: 'Retry transcript', exact: true }).click()
        await expect.poll(() => failed.length).toBe(2)
        await expect(surface.getByRole('alert')).toContainText('Transcript failed to load.')
        expect(failed[1]).toBe(failed[0])
      }
      const beforeGesture = headers.length
      const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
      await transcript.hover()
      await page.mouse.wheel(0, -900)
      await expect.poll(() => reads.length).toBe(expectedReads - 1 + itemLimit)
      await expect(surface).toHaveAttribute('aria-busy', 'false')
      await expect(surface.getByRole('alert')).toHaveCount(0)
      expect(headers[beforeGesture]?.searchParams.get('max_items')).toBe('8')
      expect(failed).toHaveLength(retry === 'gesture' ? 1 : 2)
      return
    }
    if (retry) {
      for (const count of [1, 2]) {
        await expect(surface.getByRole('alert')).toContainText('Transcript failed to load.')
        expect(failed).toHaveLength(count)
        expect(reads).toHaveLength(expectedReads - 1)
        await surface.getByRole('button', { name: 'Retry transcript', exact: true }).click()
        if (count === 1) await expect.poll(() => failed.length).toBe(2)
      }
      expect(failed[1]).toBe(failed[0])
    }
    await expect.poll(() => reads.length).toBe(expectedReads)
    await expect(surface).toHaveAttribute('aria-busy', 'false')
    await expect(surface.getByRole('alert')).toHaveCount(0)
    if (retry) {
      expect(headers.at(-1)?.search).toBe(failed[0])
      expect(reads.at(-1)?.searchParams.get('max_bytes')).toBe(
        String(byteLimit - (expectedReads - 1) * 128),
      )
    }
    await expect(
      surface.getByText(
        'No messages in this part of the conversation. Keep scrolling to look for messages.',
      ),
    ).toBeVisible()
    expect(reads).toHaveLength(expectedReads)
    expect(reads.length * 128).toBeLessThanOrEqual(byteLimit)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    const beforeGesture = headers.length
    await transcript.hover()
    await page.mouse.wheel(0, -900)
    await expect.poll(() => reads.length).toBeGreaterThan(expectedReads)
    expect(headers[beforeGesture]?.searchParams.get('max_items')).toBe(
      String(Math.min(8, itemLimit, Math.floor(byteLimit / minDetailBytes))),
    )
  })
}

for (const budget of [8, 128]) {
  test(`serializes rapid edge events and scans hidden later windows within the ${budget}-item budget`, async ({
    page,
  }) => {
    const problems: string[] = []
    page.on('pageerror', (error) => problems.push(error.message))
    page.on('console', (message) => {
      if (message.type() === 'error') problems.push(message.text())
    })
    const after: string[] = []
    let release = () => {}
    const nextPage = new Promise<void>((resolve) => {
      release = resolve
    })
    const hidden = (sequence: string) => Number(sequence) > 8 && Number(sequence) <= 24
    await page.route('**/api/**', async (route) => {
      const url = new URL(route.request().url())
      if (url.pathname.endsWith('/follow'))
        return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
      if (url.pathname === '/api/attention')
        return route.fulfill({
          json: { cursor: '0', summaries: [], continuation_after_session_id: null },
        })
      const payload = transcriptFixture(url, 48)
      if (url.pathname === '/api/bootstrap') {
        const bootstrap =
          payload as typeof import('../src/product.fixture').webContractBootstrapFixture
        return route.fulfill({
          json: {
            ...bootstrap,
            limits: { ...bootstrap.limits, max_timeline_detail_items: budget },
          },
        })
      }
      if (url.pathname.endsWith('/timeline')) {
        if (url.searchParams.get('anchor') === 'after') {
          after.push(url.searchParams.get('address') ?? '')
          if (url.searchParams.get('address') === '8') await nextPage
        }
        const window =
          payload as import('../src/generated/web-contract.mjs').WebSessionTimelineWindow
        return route.fulfill({
          json: {
            ...window,
            items: window.items.map((item) =>
              hidden(item.address.event_sequence) ? { ...item, kind: 'turn_completed' } : item,
            ),
          },
        })
      }
      if (
        url.pathname.endsWith('/timeline-detail') &&
        hidden(url.searchParams.get('first') ?? '0')
      ) {
        const detail =
          payload as import('../src/generated/web-contract.mjs').WebSessionTimelineDetailPage
        return route.fulfill({
          json: {
            ...detail,
            projected_body_bytes: 128,
            items: detail.items.map((item) => ({
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
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
    await expect(transcript.getByText('Message 48', { exact: true })).toBeVisible()
    await page.getByRole('button', { name: /^First/ }).click()
    await expect(transcript.getByText('Message 1', { exact: true })).toBeVisible()
    await expect(surface).toHaveAttribute('aria-busy', 'false')
    await transcript.evaluate((element) => {
      element.scrollTop = element.scrollHeight
      for (let index = 0; index < 40; index++) {
        element.dispatchEvent(new WheelEvent('wheel', { deltaY: 900, bubbles: true }))
        element.dispatchEvent(new Event('scroll'))
      }
    })
    await expect.poll(() => after.length).toBe(1)
    await expect(surface).toHaveAttribute('aria-busy', 'true')
    release()
    await expect(surface).toHaveAttribute('aria-busy', 'false')
    if (budget === 8) {
      // A hidden eight-item page exhausts this scan; each new gesture gets a fresh budget.
      await page.waitForTimeout(200)
      expect(after).toEqual(['8'])
      for (const address of ['16', '24']) {
        await transcript.evaluate((element) => {
          element.scrollTop = element.scrollHeight
          element.dispatchEvent(new WheelEvent('wheel', { deltaY: 900, bubbles: true }))
        })
        await expect.poll(() => after).toContain(address)
        await expect(surface).toHaveAttribute('aria-busy', 'false')
      }
    }
    await expect(transcript.getByText('Message 25', { exact: true })).toBeVisible()
    await expect(surface).toHaveAttribute('aria-busy', 'false')
    await page.waitForTimeout(200)
    expect(after).toEqual(['8', '16', '24'])
    expect(Number(await transcript.getAttribute('data-total-loaded'))).toBeLessThanOrEqual(24)
    expect(problems).toEqual([])
  })
}

test('preserves the reading offset when older rows move the selected row index', async ({
  page,
}, testInfo) => {
  const problems: string[] = []
  page.on('pageerror', (error) => problems.push(error.message))
  page.on('console', (message) => {
    if (message.type() === 'error') problems.push(message.text())
  })
  let requestedBefore = false
  let releaseBefore = () => {}
  const before = new Promise<void>((resolve) => {
    releaseBefore = resolve
  })
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    if (url.pathname.endsWith('/timeline') && url.searchParams.get('anchor') === 'before') {
      requestedBefore = true
      await before
    }
    return route.fulfill({ json: transcriptFixture(url) })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const surface = page.getByRole('region', { name: 'Transcript text', exact: true })
  await expect(transcript.getByText('Message 100000', { exact: true })).toBeVisible()
  await page.evaluate((sessionId) => {
    const preferences = JSON.parse(localStorage.getItem('signalbox.web.preferences.v1') ?? '{}')
    preferences.lastLogicalPositions = { ...preferences.lastLogicalPositions, [sessionId]: '500' }
    localStorage.setItem('signalbox.web.preferences.v1', JSON.stringify(preferences))
  }, transcriptSessionId)
  await page.reload()
  // Keep row measurements fixed so this regression isolates selection-driven scrolling.
  await page.addStyleTag({ content: '.session-message-entry { height: 100px; }' })
  await expect(transcript.getByText('Message 500', { exact: true })).toBeVisible()
  await expect(surface).toHaveAttribute('aria-busy', 'false')
  await transcript.evaluate((element) => {
    element.scrollTop = 0
    element.dispatchEvent(new WheelEvent('wheel', { deltaY: -900, bubbles: true }))
  })
  await expect.poll(() => requestedBefore).toBe(true)
  const anchor = transcript.getByText('Message 496', { exact: true })
  await expect(anchor).toBeVisible()
  const top = await anchor.evaluate((element) => element.getBoundingClientRect().top)
  releaseBefore()
  await expect(transcript).toHaveAttribute('data-total-loaded', '16')
  await expect(surface).toHaveAttribute('aria-busy', 'false')
  await page.waitForTimeout(500)
  expect(
    Math.abs((await anchor.evaluate((element) => element.getBoundingClientRect().top)) - top),
  ).toBeLessThan(2)
  await page.screenshot({ path: testInfo.outputPath('selected-row-prepend.png') })
  await page.getByRole('button', { name: /^First/ }).click()
  await expect(transcript.getByText('Message 1', { exact: true })).toBeVisible()
  expect(problems).toEqual([])
})

test('follows a taller live window that replaces every retained row', async ({
  page,
}, testInfo) => {
  const problems: string[] = []
  page.on('pageerror', (error) => problems.push(error.message))
  page.on('console', (message) => {
    if (message.type() === 'error') problems.push(message.text())
  })
  let latest = 8
  let releaseGrowth = () => {}
  const growth = new Promise<void>((resolve) => {
    releaseGrowth = resolve
  })
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname === `/api/sessions/${transcriptSessionId}/follow`) {
      await growth
      return route.fulfill({
        contentType: 'application/x-ndjson',
        body:
          [
            {
              kind: 'snapshot',
              snapshot: {
                session_id: transcriptSessionId,
                observed_through: '8',
                active: null,
                queued_turn_count: '0',
                queued_turn_ids: [],
                reconciliation: null,
                runner: null,
              },
            },
            {
              kind: 'durable',
              cursor: '16',
              address: { event_sequence: '16' },
              event_kind: 'input_accepted',
            },
          ]
            .map((event) => JSON.stringify(event))
            .join('\n') + '\n',
      })
    }
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    const payload = transcriptFixture(url, latest)
    if (url.pathname.endsWith('/timeline-detail') && url.searchParams.get('first') === '16') {
      const detail =
        payload as import('../src/generated/web-contract.mjs').WebSessionTimelineDetailPage
      const text = 'New tail line\n'.repeat(40) + 'Replacement tail'
      return route.fulfill({
        json: {
          ...detail,
          projected_body_bytes: 128 + text.length,
          items: detail.items.map((item) => ({
            ...item,
            projected_body_bytes: 128 + text.length,
            body: {
              ...item.body,
              text: {
                text,
                offset_bytes: '0',
                total_bytes: String(text.length),
                continuation: null,
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
  const remaining = () =>
    transcript.evaluate(
      (element) => element.scrollHeight - element.scrollTop - element.clientHeight,
    )
  await expect(transcript.getByText('Message 8', { exact: true })).toBeVisible()
  await expect.poll(remaining).toBeLessThanOrEqual(1)
  const oldHeight = await transcript.evaluate((element) => element.scrollHeight)
  latest = 16
  releaseGrowth()
  await expect(transcript.getByText(/Replacement tail/)).toBeVisible()
  await expect(transcript.getByText('Message 8', { exact: true })).toHaveCount(0)
  await expect
    .poll(() => transcript.evaluate((element) => element.scrollHeight))
    .toBeGreaterThan(oldHeight)
  await expect.poll(remaining).toBeLessThanOrEqual(1)
  await expect(transcript).toHaveAttribute('data-total-loaded', '8')
  await page.screenshot({ path: testInfo.outputPath('replacement-tail.png') })
  expect(problems).toEqual([])
})

const hiddenDetailCases: {
  kind: string
  field: WebTimelineBodyField
  body: (excerpt: WebTimelineTextExcerpt) => WebSessionTimelineDetailBody
}[] = [
  {
    kind: 'goal_changed',
    field: 'goal_text',
    body: (text) => ({
      type: 'goal_event',
      session_id: transcriptSessionId,
      event: { type: 'blocked', generation: '1', reason: 'user_input_required', text },
    }),
  },
  {
    kind: 'context_compacted',
    field: 'compaction_summary',
    body: (summary) => ({
      type: 'context_compaction',
      compaction_id: transcriptSessionId,
      model_call_id: transcriptSessionId,
      through_position: '1',
      summary_entry_id: transcriptSessionId,
      result_frontier_id: transcriptSessionId,
      summary,
    }),
  },
  {
    kind: 'delegation_update',
    field: 'delegation_content',
    body: (content) => ({
      type: 'delegation',
      detail: {
        type: 'session_message',
        relationship_id: transcriptSessionId,
        message_id: transcriptSessionId,
        sender_session_id: '00000000-0000-0000-0000-000000000992',
        recipient_session_id: transcriptSessionId,
        delivery_sequence: '1',
        message_ordinal: '1',
        content,
      },
    }),
  },
  {
    kind: 'tool_approval_decided',
    field: 'approval_rationale',
    body: (rationale) => ({
      type: 'tool_approval_decision',
      turn_id: transcriptSessionId,
      request_id: transcriptSessionId,
      tool_name: 'exec_command',
      decision: 'approve',
      actor: { type: 'user', command_id: transcriptSessionId },
      approval_judge_escalated: false,
      rationale,
    }),
  },
]
for (const hidden of hiddenDetailCases) {
  test(`retains a readable continuation for hidden ${hidden.kind} details`, async ({
    page,
  }, testInfo) => {
    const reads: (string | null)[] = []
    const problems: string[] = []
    page.on('pageerror', (error) => problems.push(error.message))
    page.on('console', (message) => {
      if (message.type() === 'error') problems.push(message.text())
    })
    const suffix = 'Continued metadata text'
    const cursor = {
      address: { event_sequence: '2' },
      field: hidden.field,
      member_index: 0,
      offset_bytes: '6',
    }
    await page.route('**/api/**', (route) => {
      const url = new URL(route.request().url())
      if (url.pathname.endsWith('/follow'))
        return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
      if (url.pathname === '/api/attention')
        return route.fulfill({
          json: { cursor: '0', summaries: [], continuation_after_session_id: null },
        })
      const payload = transcriptFixture(url, 2)
      if (url.pathname === '/api/bootstrap') {
        const bootstrap =
          payload as typeof import('../src/product.fixture').webContractBootstrapFixture
        return route.fulfill({
          json: { ...bootstrap, limits: { ...bootstrap.limits, max_timeline_detail_items: 1 } },
        })
      }
      if (url.pathname.endsWith('/timeline')) {
        const window =
          payload as import('../src/generated/web-contract.mjs').WebSessionTimelineWindow
        const items = window.items.map((item) =>
          item.address.event_sequence === '2'
            ? { ...item, kind: hidden.kind, projected_structured_bytes: 64 + hidden.kind.length }
            : item,
        )
        return route.fulfill({
          json: {
            ...window,
            items,
            projected_structured_bytes: items.reduce(
              (sum, item) => sum + item.projected_structured_bytes,
              0,
            ),
          },
        })
      }
      if (url.pathname.endsWith('/timeline-detail')) {
        reads.push(url.searchParams.get('cursor_field'))
        if (url.searchParams.get('first') === '2') {
          const continued = url.searchParams.has('cursor_field')
          const text = continued ? suffix : 'Start '
          return route.fulfill({
            json: {
              session_id: transcriptSessionId,
              projected_body_bytes: 128 + text.length,
              continuation: continued ? null : { type: 'more_body', body: cursor },
              items: [
                {
                  address: cursor.address,
                  kind: hidden.kind,
                  projected_body_bytes: 128 + text.length,
                  body: hidden.body({
                    text,
                    offset_bytes: continued ? '6' : '0',
                    total_bytes: String(6 + suffix.length),
                    continuation: continued ? null : cursor,
                  }),
                },
              ],
            },
          })
        }
      }
      return route.fulfill({ json: payload })
    })
    await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    const row = transcript.locator('[data-event-sequence="2"]')
    await expect(row.getByRole('button', { name: 'Read more', exact: true })).toBeVisible()
    await page.waitForTimeout(200)
    expect(reads).toEqual([null])
    await expect(transcript).toHaveAttribute('data-total-loaded', '1')
    await row.getByRole('button', { name: 'Read more', exact: true }).click()
    await expect(row.getByRole('region', { name: 'Details', exact: true })).toContainText(suffix)
    expect(reads).toEqual([null, hidden.field])
    await expect(row.getByRole('button', { name: 'Continue reading', exact: true })).toHaveCount(0)
    await page.screenshot({ path: testInfo.outputPath('hidden-detail-continuation.png') })
    expect(problems).toEqual([])
  })
}
