import { expect, test } from './fontTest'
import { openSession, sessionApi } from './session-fixture'

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
