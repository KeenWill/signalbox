import { expect, test } from '@playwright/test'
import { detailFixture, templateFixture } from './fixtures'

const scenario = '/src/features/templates/scenario.html'
test.beforeEach(async ({ page }) => {
  await page.route('**/api/templates', (route) =>
    route.fulfill({ json: { templates: [templateFixture] } }),
  )
  await page.route('**/api/templates/code-review', (route) =>
    route.fulfill({ json: detailFixture }),
  )
})

test('opens the definition using the keyboard', async ({ page }, testInfo) => {
  await page.goto(scenario)
  const row = page.getByRole('button', { name: /code-review/ })
  await expect(row).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('template-list.png'), fullPage: true })
  await row.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('heading', { name: 'code-review' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Start session' })).toBeDisabled()
  await page.getByText('System instructions', { exact: true }).click()
  await expect(page.getByText(detailFixture.system_prompt, { exact: true })).toBeVisible()
  await page.getByText('Definition and digest', { exact: true }).click()
  await expect(page.getByText(templateFixture.digest, { exact: true })).toBeVisible()
  await page.screenshot({ path: testInfo.outputPath('template-detail.png'), fullPage: true })
  await page.getByRole('button', { name: 'Back to templates' }).click()
  await expect(row).toBeVisible()
  await expect(row).toBeFocused()
})

test('fits a phone viewport', async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(scenario)
  await page.getByRole('button', { name: /code-review/ }).click()
  await expect(page.getByRole('heading', { name: 'code-review' })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390)
  await page.screenshot({ path: testInfo.outputPath('template-phone.png'), fullPage: true })
})

test('virtualizes a large loaded catalog', async ({ page }) => {
  const templates = Array.from({ length: 10000 }, (_, index) => ({
    ...templateFixture,
    name: `template-${index}`,
  }))
  await page.route('**/api/templates', (route) => route.fulfill({ json: { templates } }))
  await page.goto(scenario)
  await expect(page.getByRole('button', { name: /template-0 / })).toBeVisible()
  expect(await page.getByRole('button').count()).toBeLessThan(40)
})

test('restores list focus after browser Back', async ({ page }) => {
  await page.goto(scenario)
  const row = page.getByRole('button', { name: /code-review/ })
  await row.focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/templates#code-review$/)
  await expect(page.getByRole('heading', { name: 'code-review' })).toBeVisible()
  await page.goBack()
  await expect(row).toBeFocused()
  await page.goForward()
  await expect(page.getByRole('heading', { name: 'code-review' })).toBeVisible()
  await page.goBack()
  await expect(row).toBeFocused()
})

test('Tab reveals rows beyond the initial virtual range', async ({ page }) => {
  await page.setViewportSize({ width: 900, height: 300 })
  const templates = Array.from({ length: 200 }, (_, index) => ({
    ...templateFixture,
    name: `template-${index}`,
  }))
  await page.route('**/api/templates', (route) => route.fulfill({ json: { templates } }))
  await page.goto(scenario)
  await page.getByRole('button', { name: /^template-0 / }).focus()
  const targetIndex = 40
  for (let index = 0; index < targetIndex; index += 1) await page.keyboard.press('Tab')
  await expect(
    page.getByRole('button', { name: new RegExp(`^template-${targetIndex} `) }),
  ).toBeFocused()
})

test('shows validation inline and adopts a successful saved definition', async ({
  page,
}, testInfo) => {
  const updatedPrompt = 'Review correctness and report the evidence.'
  const updated = {
    ...detailFixture,
    summary: { ...templateFixture, digest: '2'.repeat(64) },
    system_prompt: updatedPrompt,
    definition_toml: detailFixture.definition_toml.replace(
      detailFixture.system_prompt,
      updatedPrompt,
    ),
  }
  let current = detailFixture
  await page.route('**/api/templates/code-review', async (route) => {
    if (route.request().method() === 'PUT') {
      const body = route.request().postDataJSON()
      if (body.definition_toml === 'invalid definition') {
        await route.fulfill({
          status: 422,
          json: {
            error: {
              kind: 'application',
              code: 'invalid_template_edit',
              message: 'Template definition is not valid TOML',
            },
          },
        })
        return
      }
      expect(body).toEqual({ definition_toml: updated.definition_toml })
      current = updated
    }
    await route.fulfill({ json: current })
  })
  await page.goto(scenario)
  await page.getByRole('button', { name: /code-review/ }).click()
  await page.getByText('Edit template', { exact: true }).click()
  const editor = page.getByRole('textbox', { name: 'Template definition (TOML)' })
  await editor.fill('invalid definition')
  await page.getByRole('button', { name: 'Save template' }).click()
  await expect(page.getByRole('alert')).toHaveText('Template definition is not valid TOML')
  await expect(editor).toHaveValue('invalid definition')
  await page.screenshot({ path: testInfo.outputPath('template-edit-error.png'), fullPage: true })
  await editor.fill(updated.definition_toml)
  await page.getByRole('button', { name: 'Save template' }).click()
  await expect(page.getByRole('status')).toHaveText('Template saved.')
  await expect(page.getByRole('alert')).toHaveCount(0)
  await page.getByText('Definition and digest', { exact: true }).click()
  await expect(page.getByText(updated.summary.digest, { exact: true })).toBeVisible()
  await page.getByText('System instructions', { exact: true }).click()
  await expect(page.getByText(updatedPrompt, { exact: true })).toBeVisible()
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390)
  await page.screenshot({ path: testInfo.outputPath('template-edit-phone.png'), fullPage: true })
})

test('requires reconciling a refreshed definition before saving an edited draft', async ({
  page,
}, testInfo) => {
  let current = detailFixture
  const saves: string[] = []
  await page.route('**/api/templates/code-review', (route) => {
    if (route.request().method() === 'PUT') {
      saves.push(route.request().postDataJSON().definition_toml)
    }
    return route.fulfill({ json: current })
  })
  await page.goto(scenario)
  await page.getByRole('button', { name: /code-review/ }).click()
  await page.getByText('Edit template', { exact: true }).click()
  const editor = page.getByRole('textbox', { name: 'Template definition (TOML)' })
  await expect(editor).toHaveValue(detailFixture.definition_toml)
  await editor.fill('An edit that will be reverted.')
  await editor.fill(detailFixture.definition_toml)
  const refreshedPrompt = 'Instructions updated in another browser.'
  current = {
    ...detailFixture,
    summary: { ...detailFixture.summary, digest: '2'.repeat(64) },
    system_prompt: refreshedPrompt,
    definition_toml: detailFixture.definition_toml.replace(
      detailFixture.system_prompt,
      refreshedPrompt,
    ),
  }
  await page.evaluate(() => window.dispatchEvent(new Event('visibilitychange')))
  await expect(editor).toHaveValue(current.definition_toml)
  const draft = 'Instructions being edited locally.'
  await editor.fill(draft)
  const laterPrompt = 'Another external update.'
  current = {
    ...current,
    system_prompt: laterPrompt,
    definition_toml: current.definition_toml.replace(refreshedPrompt, laterPrompt),
  }
  await page.evaluate(() => window.dispatchEvent(new Event('visibilitychange')))
  await page.getByText('System instructions', { exact: true }).click()
  await expect(page.getByText(laterPrompt, { exact: true })).toBeVisible()
  await expect(editor).toHaveValue(draft)
  const save = page.getByRole('button', { name: 'Save template', exact: true })
  await expect(save).toBeDisabled()
  await expect(page.getByRole('alert')).toContainText(
    'This template changed while you were editing.',
  )
  await editor.fill(`${draft} More local edits.`)
  await expect(save).toBeDisabled()
  await save.evaluate((button: HTMLButtonElement) => button.form?.requestSubmit())
  expect(saves).toEqual([])
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390)
  await page.screenshot({
    path: testInfo.outputPath('template-edit-conflict-phone.png'),
    fullPage: true,
  })
  await page.getByRole('button', { name: 'Discard edits and use latest' }).click()
  await expect(editor).toHaveValue(current.definition_toml)
  await expect(editor).toBeFocused()
  await expect(page.getByRole('alert')).toHaveCount(0)
  await expect(save).toBeEnabled()
  await save.click()
  await expect(page.getByRole('status')).toHaveText('Template saved.')
  expect(saves).toEqual([current.definition_toml])
})

test('follows refreshes after another browser saves the local draft', async ({ page }) => {
  let current = detailFixture
  await page.route('**/api/templates/code-review', (route) => route.fulfill({ json: current }))
  await page.goto(scenario)
  await page.getByRole('button', { name: /code-review/ }).click()
  await page.getByText('Edit template', { exact: true }).click()
  await page.getByText('System instructions', { exact: true }).click()
  const editor = page.getByRole('textbox', { name: 'Template definition (TOML)' })
  const matchingPrompt = 'Instructions also saved by another browser.'
  const matchingSource = detailFixture.definition_toml.replace(
    detailFixture.system_prompt,
    matchingPrompt,
  )
  await editor.fill(matchingSource)
  current = {
    ...detailFixture,
    summary: { ...detailFixture.summary, digest: '2'.repeat(64) },
    system_prompt: matchingPrompt,
    definition_toml: matchingSource,
  }
  await page.evaluate(() => window.dispatchEvent(new Event('visibilitychange')))
  await expect(page.getByText(matchingPrompt, { exact: true })).toBeVisible()
  await expect(editor).toHaveValue(matchingSource)
  await expect(page.getByRole('alert')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Save template', exact: true })).toBeEnabled()
  const laterPrompt = 'A later change after the matching save.'
  current = {
    ...current,
    summary: { ...current.summary, digest: '3'.repeat(64) },
    system_prompt: laterPrompt,
    definition_toml: matchingSource.replace(matchingPrompt, laterPrompt),
  }
  await page.evaluate(() => window.dispatchEvent(new Event('visibilitychange')))
  await expect(editor).toHaveValue(current.definition_toml)
  await expect(page.getByRole('alert')).toHaveCount(0)
})
