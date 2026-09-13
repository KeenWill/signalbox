import { transcriptFixture, transcriptSessionId } from '../src/session-timeline/transcript.fixture'
import { expect, test } from './fontTest'

for (const width of [1440, 390]) {
  test(`scrolls through middle event windows in both directions with bounded rows at ${width}`, async ({
    page,
  }, testInfo) => {
    await page.setViewportSize({ width, height: 900 })
    const windows: { anchor: string | null; address: string | null }[] = []
    await page.route('**/api/**', (route) => {
      const url = new URL(route.request().url())
      if (url.pathname.endsWith('/follow'))
        return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
      if (url.pathname === '/api/attention')
        return route.fulfill({
          json: { cursor: '0', summaries: [], continuation_after_session_id: null },
        })
      if (url.pathname.endsWith('/timeline') && url.searchParams.get('max_items') === '80') {
        expect(url.searchParams.get('max_bytes')).toBe('65536')
        windows.push({
          anchor: url.searchParams.get('anchor'),
          address: url.searchParams.get('address'),
        })
      }
      return route.fulfill({ json: transcriptFixture(url) })
    })
    await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
    await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
    const events = page.getByRole('grid', { name: 'Session timeline', exact: true })
    await expect(events.getByRole('row')).toHaveCount(80)
    await page.getByRole('button', { name: 'First', exact: true }).click()
    await expect(events.getByText('1', { exact: true })).toBeInViewport()
    await expect(events).not.toHaveAttribute('aria-busy', 'true')
    for (const first of [81, 161, 241]) {
      await events.evaluate((element) => {
        element.scrollTop = element.scrollHeight
      })
      await expect(events.getByText(String(first), { exact: true })).toBeInViewport()
      await expect(events).not.toHaveAttribute('aria-busy', 'true')
      await expect(events.getByRole('row')).toHaveCount(80)
    }
    await page.screenshot({ path: testInfo.outputPath(`events-middle-${width}.png`) })
    for (const last of [240, 160, 80]) {
      await events.evaluate((element) => {
        element.scrollTop = 0
        element.dispatchEvent(new WheelEvent('wheel', { deltaY: -900, bubbles: true }))
      })
      await expect(events.getByText(String(last), { exact: true })).toBeInViewport()
      await expect(events).not.toHaveAttribute('aria-busy', 'true')
      await expect(events.getByRole('row')).toHaveCount(80)
    }
    expect(windows).toEqual([
      { anchor: 'latest', address: null },
      { anchor: 'first', address: null },
      { anchor: 'after', address: '80' },
      { anchor: 'after', address: '160' },
      { anchor: 'after', address: '240' },
      { anchor: 'before', address: '241' },
      { anchor: 'before', address: '161' },
      { anchor: 'before', address: '81' },
    ])
    await page.getByRole('button', { name: 'Latest', exact: true }).click()
    await expect(events.getByText('100000', { exact: true })).toBeInViewport()
    await expect(page.getByRole('button', { name: 'Previous', exact: true })).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Next', exact: true })).toHaveCount(0)
  })
}

test('traverses short event windows with keyboard and touch while sharing pending edge reads', async ({
  page,
}) => {
  let afterReads = 0
  let release = () => {}
  const held = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    if (url.pathname.endsWith('/timeline') && url.searchParams.get('max_items') === '80') {
      if (url.searchParams.get('anchor') === 'after') {
        afterReads += 1
        if (url.searchParams.get('address') === '4') await held
      }
      url.searchParams.set('max_items', '2')
    }
    return route.fulfill({ json: transcriptFixture(url) })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  const events = page.getByRole('grid', { name: 'Session timeline', exact: true })
  await page.getByRole('button', { name: 'First', exact: true }).click()
  await expect(events.getByText('1', { exact: true })).toBeVisible()
  await events.focus()
  await events.press('PageDown')
  await expect(events.getByText('3', { exact: true })).toBeVisible()
  await expect(events).toBeFocused()
  await events.press('PageUp')
  await expect(events.getByText('2', { exact: true })).toBeVisible()
  await events.evaluate((element) => {
    for (const [type, clientY] of [
      ['touchstart', 200],
      ['touchmove', 100],
    ] as const) {
      const event = new Event(type, { bubbles: true })
      Object.defineProperty(event, 'touches', { value: [{ clientY }] })
      element.dispatchEvent(event)
    }
  })
  await expect(events.getByText('3', { exact: true })).toBeVisible()
  await expect(events).not.toHaveAttribute('aria-busy', 'true')
  await events.evaluate((element) => {
    for (let i = 0; i < 5; i += 1)
      element.dispatchEvent(new WheelEvent('wheel', { deltaY: 900, bubbles: true }))
  })
  await expect(events).toHaveAttribute('aria-busy', 'true')
  await expect.poll(() => afterReads).toBe(3)
  release()
  await expect(events.getByText('5', { exact: true })).toBeVisible()
  await expect(events).not.toHaveAttribute('aria-busy', 'true')
  await expect(events.getByRole('row')).toHaveCount(2)
  expect(afterReads).toBe(3)
  await expect(events).toBeFocused()
})
