import { expect, test } from '@playwright/test'
import {
  argumentExamples,
  fileEvidence,
  jsonExamples,
  longEvidence,
  rawFileEvidence,
} from './toolScenario'

test('tool summaries expose raw evidence by keyboard', async ({ page }, testInfo) => {
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(error.message))
  await page.goto('/src/features/tools/scenario.html')
  await expect(page.getByText('Exit 0')).toBeVisible()
  const file = page.getByRole('article', { name: 'Tool read_file', exact: true }).last()
  expect(
    await file.getByRole('region', { name: 'File contents' }).locator('code').textContent(),
  ).toBe(fileEvidence)
  await file.getByRole('button', { name: 'Raw', exact: true }).click()
  expect(
    await file.getByRole('region', { name: 'Output', exact: true }).locator('code').textContent(),
  ).toBe(rawFileEvidence)
  await file.getByRole('button', { name: 'Raw', exact: true }).click()
  const proposedDiff = page
    .getByRole('region', { name: 'Proposed changes', exact: true })
    .locator('code')
  expect(await proposedDiff.textContent()).toBe('- Hello\n+ Welcome')
  expect(await proposedDiff.innerText()).toBe('- Hello\n+ Welcome')
  const gitDiff = page.getByRole('region', { name: 'Diff', exact: true }).locator('code')
  expect(await gitDiff.textContent()).toBe('-before\n+after\n')
  expect(await gitDiff.innerText()).toBe('-before\n+after\n')
  const unknown = page.getByRole('article', { name: 'Tool custom_tool', exact: true }).first()
  await expect(unknown.getByText('Greeting')).toBeVisible()
  const raw = unknown.getByRole('button', { name: 'Raw' })
  await raw.focus()
  await page.keyboard.press('Enter')
  await expect(raw).toHaveAttribute('aria-expanded', 'true')
  await expect(unknown.getByText('{"greeting":"Hello"}')).toBeVisible()
  await page.keyboard.press('Enter')
  await expect(raw).toHaveAttribute('aria-expanded', 'false')
  await expect(page.getByRole('link', { name: 'Rust documentation' })).toHaveAttribute(
    'href',
    'https://doc.rust-lang.org/',
  )
  await page.screenshot({ path: testInfo.outputPath('tool-renderers.png'), fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.getByText('Exit 0')).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('tool-renderers-phone.png'), fullPage: true })
  expect(errors).toEqual([])
})

test('raw excerpts expose the complete fetched text on demand', async ({ page }) => {
  await page.goto('/src/features/tools/scenario.html')
  const tool = page.getByRole('article', { name: 'Tool long_evidence', exact: true }).last()
  await tool.getByRole('button', { name: 'Raw', exact: true }).click()
  const code = tool.getByRole('region', { name: 'Output', exact: true }).locator('code')
  await expect(code).not.toContainText('Last fetched line')
  await tool.getByRole('button', { name: 'Show all fetched text' }).click()
  expect(await code.textContent()).toBe(JSON.stringify({ content: longEvidence }))
  await tool.getByRole('button', { name: 'Show less' }).click()
  await expect(code).not.toContainText('Last fetched line')
})

for (const example of jsonExamples) {
  test(`keeps ${example.name} structured evidence behind Raw`, async ({ page }, testInfo) => {
    await page.goto('/src/features/tools/scenario.html')
    const tool = page
      .getByRole('region', { name: `Tool scenario ${example.name}`, exact: true })
      .getByRole('article', { name: `Tool ${example.name}`, exact: true })
    await expect(tool.locator('pre')).toHaveCount(0)
    expect((await tool.textContent())?.length).toBeLessThan(6000)
    expect(await tool.locator('dt').count()).toBeLessThanOrEqual(32)
    if (example.name.startsWith('json_scalar_')) {
      await expect(tool).toContainText('Output details available in Raw')
      await expect(tool.getByRole('region', { name: 'Output', exact: true })).toHaveCount(0)
    }
    if (['json_array_integer', 'json_array_overflow'].includes(example.name)) {
      await expect(tool).toContainText('Output details available in Raw')
      await expect(tool).not.toContainText('9007199254740992')
      await expect(tool).not.toContainText('Infinity')
    }
    if (['json_array_multiline', 'json_array_long'].includes(example.name))
      await expect(tool).toContainText('More in Raw')
    if (example.name === 'json_multiline_fields') {
      const values = await tool.locator('dd').allTextContents()
      expect(
        values.reduce((lines, value) => lines + value.split(/\r\n|\r|\n/u).length, 0),
      ).toBeLessThanOrEqual(32)
      await expect(tool).toContainText('Showing part of the details')
    }
    const raw = tool.getByRole('button', { name: 'Raw', exact: true })
    await raw.focus()
    await page.keyboard.press('Enter')
    const region = tool.getByRole('region', {
      name: example.name === 'json_failure' ? 'Failure' : example.label,
      exact: true,
    })
    const expand = region.getByRole('button', { name: 'Show all fetched text' })
    if (await expand.count()) await expand.click()
    expect(await region.locator('code').textContent()).toBe(example.raw)
    await raw.click()
    await expect(tool.locator('pre')).toHaveCount(0)
    if (example.name === 'json_nested') {
      await page.setViewportSize({ width: 390, height: 844 })
      await tool.scrollIntoViewIfNeeded()
      await tool.screenshot({ path: testInfo.outputPath('structured-tool-phone.png') })
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(
        true,
      )
    }
  })
}

for (const example of argumentExamples) {
  test(`labels only a completely empty object as empty for ${example.name}`, async ({ page }) => {
    await page.goto('/src/features/tools/scenario.html')
    const tool = page.getByRole('article', { name: `Tool ${example.name}`, exact: true })
    await expect(tool.getByText('No fields', { exact: true })).toHaveCount(example.empty ? 1 : 0)
    if (example.name === 'absent_arguments')
      await expect(tool).toContainText('Arguments are on another detail page')
    if (example.name === 'partial_arguments')
      await expect(tool).toContainText('Arguments excerpt available in Raw')
  })
}

test('keeps a fetched approval rationale visible without a Raw toggle', async ({ page }) => {
  await page.goto('/src/features/tools/scenario.html')
  const approval = page.getByRole('article', { name: 'Tool approval', exact: true })
  await expect(approval).toContainText('Command is outside the workspace')
  await expect(approval).toContainText('Showing part of reason')
  await expect(approval).not.toContainText('available in Raw')
  await expect(approval.getByRole('button', { name: 'Raw', exact: true })).toHaveCount(0)
})

test('labels a completely parsed empty structured file body', async ({ page }) => {
  await page.goto('/src/features/tools/scenario.html?empty-read')
  const tool = page
    .getByRole('region', { name: 'Empty file read scenario', exact: true })
    .getByRole('article')
  await expect(tool.getByRole('region', { name: 'File contents', exact: true })).toHaveText(
    'No fields',
  )
  await expect(tool.locator('pre')).toHaveCount(0)
  await tool.getByRole('button', { name: 'Raw', exact: true }).click()
  await expect(
    tool.getByRole('region', { name: 'Output', exact: true }).locator('code'),
  ).toHaveText('{"status":"structured","body":{},"truncated":false,"cursor":null}')
})

test('labels omitted scalar file contents and preserves them in Raw', async ({ page }) => {
  await page.goto('/src/features/tools/scenario.html')
  const tool = page
    .getByRole('region', { name: 'Scalar file read scenario', exact: true })
    .getByRole('article')
  const content = tool.getByRole('region', { name: 'File contents', exact: true })
  await expect(content).toContainText('Showing part of the text')
  expect((await content.locator('p').textContent())?.length).toBe(4000)
  await tool.getByRole('button', { name: 'Raw', exact: true }).click()
  const output = tool.getByRole('region', { name: 'Output', exact: true })
  await output.getByRole('button', { name: 'Show all fetched text' }).click()
  await expect(output.locator('code')).toContainText('x'.repeat(4001))
})

test('labels an empty structured string and preserves it in Raw', async ({ page }) => {
  await page.goto('/src/features/tools/scenario.html')
  const tool = page
    .getByRole('region', { name: 'Empty text file read scenario', exact: true })
    .getByRole('article')
  await expect(tool.getByRole('region', { name: 'File contents', exact: true })).toHaveText(
    'Empty text',
  )
  await tool.getByRole('button', { name: 'Raw', exact: true }).click()
  await expect(
    tool.getByRole('region', { name: 'Output', exact: true }).locator('code'),
  ).toHaveText('{"status":"structured","body":"","truncated":false,"cursor":null}')
})
