import { expect, test } from './fontTest'
import { openSession, sessionApi, sessionId, turnId } from './session-fixture'

for (const viewport of [
  { name: 'desktop', width: 1440, height: 1000 },
  { name: 'phone', width: 390, height: 844 },
]) {
  test(`compact header ${viewport.name}`, async ({ page }, testInfo) => {
    await page.setViewportSize(viewport)
    await sessionApi(page)
    await openSession(page)
    await expect(page.getByRole('textbox', { name: 'Session ID' })).toHaveCount(0)
    await expect(page.getByRole('heading', { name: 'Session', exact: true })).toBeVisible()
    const box = await page.locator('.session-compact-header').boundingBox()
    expect(box?.height).toBeLessThanOrEqual(70)
    await page.getByRole('textbox', { name: 'Message', exact: true }).press('Escape')
    await expect(page.getByRole('region', { name: 'Conversation', exact: true })).toBeFocused()
    await page.screenshot({
      path: testInfo.outputPath(`header-${viewport.name}.png`),
      fullPage: true,
    })
    await page.getByText('Session details', { exact: true }).click()
    await expect(page.getByText('Up to date as of', { exact: true })).toBeVisible()
    await page.getByText('Session details', { exact: true }).press('Escape')
    await expect(page.getByText('Up to date as of', { exact: true })).toBeHidden()
    await expect(page.getByText('Session details', { exact: true })).toBeFocused()
    await expect(page.getByRole('heading', { name: 'Session', exact: true })).toBeVisible()
  })
}

test('Escape closes trigger details before session details', async ({ page }) => {
  await sessionApi(page, false, sessionId, {
    dispatch_id: turnId,
    action_ordinal: '1',
    repository: 'signalbox/example',
    pull_request: '81',
    rule_id: 'review-response',
    rule_revision: '3',
    event_id: sessionId,
    event_kind: 'review_submitted',
  })
  await openSession(page)
  const sessionDetails = page.getByText('Session details', { exact: true })
  const triggerDetails = page.getByText('Trigger details', { exact: true })
  await sessionDetails.click()
  await triggerDetails.click()
  const trigger = page.getByText(`Trigger ${turnId} · Action 1`, { exact: true })
  await expect(trigger).toBeVisible()
  await triggerDetails.press('Escape')
  await expect(trigger).toBeHidden()
  await expect(triggerDetails).toBeFocused()
  await expect(page.getByText('Up to date as of', { exact: true })).toBeVisible()
  await triggerDetails.press('Escape')
  await expect(page.getByText('Up to date as of', { exact: true })).toBeHidden()
  await expect(sessionDetails).toBeFocused()
  await expect(page.getByRole('heading', { name: 'Session', exact: true })).toBeVisible()
})
