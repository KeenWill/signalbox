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
