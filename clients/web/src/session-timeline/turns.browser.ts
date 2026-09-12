import { expect, test } from '../../e2e/fontTest'
import { detailTurnId } from '../../e2e/session-detail-fixture'
import { turnApi } from './turns.fixture'
import '../../e2e/session-transcript-turns.spec'

test('keeps linked events and turn targets visible', async ({ page }, testInfo) => {
  await turnApi(page)
  const turnReads: string[] = []
  page.on('request', (request) => {
    if (request.url().includes('/turns/')) turnReads.push(request.url())
  })
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await page.goto(`/src/session-timeline/transcript-scenario.html?around=3`)
  const linked = transcript.locator('[data-event-sequence="3"]')
  await expect(linked).toBeInViewport()
  await expect(linked).toBeFocused()
  await page.screenshot({ path: testInfo.outputPath('linked-turn-detail.png') })
  await page.goto(`/src/session-timeline/transcript-scenario.html?turn=${detailTurnId}`)
  await expect
    .poll(() => turnReads.some((url) => !new URL(url).searchParams.has('cursor_address')))
    .toBe(true)
  await expect(
    transcript.getByText('Inspect the release status and retain the result.'),
  ).toBeVisible()
})

test('keeps turn summaries and level controls usable at phone width', async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await turnApi(page)
  await page.goto('/src/session-timeline/transcript-scenario.html')
  await page.getByRole('radio', { name: 'Summary', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(
    transcript.getByText('The release checks passed. Publishing remains unapproved.'),
  ).toBeInViewport()
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'passed',
  )
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390)
  await page.screenshot({ path: testInfo.outputPath('turn-tools-phone.png') })
})

test('supplies loaded arguments and output to the tool renderer slot', async ({ page }) => {
  await turnApi(page)
  await page.goto('/src/session-timeline/transcript-scenario.html?renderer=true')
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const renderer = page.getByRole('region', { name: 'Injected tool renderer' })
  await expect(renderer).toContainText('release status')
  await expect(renderer).toContainText('passed')
  await expect(renderer).toHaveAttribute('data-detail', 'condensed')
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  await expect(renderer).toHaveAttribute('data-detail', 'full')
  await page.getByRole('button', { name: 'Continue reading', exact: true }).click()
  await expect(renderer).toContainText('passed')
})

test('unwinds turn expansion through the surface command and restores its focus', async ({
  page,
}) => {
  await turnApi(page)
  await page.goto('/src/session-timeline/transcript-scenario.html')
  await page.getByRole('radio', { name: 'Summary', exact: true }).check()
  const open = page.getByRole('button', { name: 'Open turn details', exact: true })
  await open.click()
  await expect(page.getByRole('button', { name: 'Collapse turn', exact: true })).toBeFocused()
  await page.keyboard.press('Escape')
  await expect(open).toBeFocused()
  await expect(page.getByRole('button', { name: 'Collapse turn', exact: true })).toHaveCount(0)
  await page.keyboard.press('Escape')
  await expect(open).toBeFocused()
})

test('restores Tools after collapsing a turn by button or Escape', async ({ page }) => {
  await turnApi(page)
  await page.goto('/src/session-timeline/transcript-scenario.html')
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const open = page.getByRole('button', { name: 'Open turn details', exact: true })
  const summary = page.getByRole('region', { name: 'exec_command details', exact: true })
  await expect(summary).toContainText('passed')
  await open.click()
  await page.getByRole('button', { name: 'Collapse turn', exact: true }).click()
  await expect(summary).toContainText('passed')
  await open.click()
  await page.keyboard.press('Escape')
  await expect(open).toBeFocused()
  await expect(summary).toContainText('passed')
  await page.keyboard.press('Escape')
  await expect(summary).toContainText('passed')
})
