import { turnApi } from '../src/session-timeline/turns.fixture'
import { expect, test } from './fontTest'
import {
  detailExcerpt,
  detailPage,
  detailSessionId,
  toolResultItem,
} from './session-detail-fixture'

test('shows final turn text and a tool chip while keeping lifecycle noise closed', async ({
  page,
}, testInfo) => {
  await turnApi(page)
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Summary', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(
    transcript.getByText('The release checks passed. Publishing remains unapproved.'),
  ).toBeVisible()
  await expect(
    transcript.getByText('Inspect the release status and retain the result.'),
  ).toBeVisible()
  await expect(transcript.getByText('Turn completed', { exact: true })).toHaveCount(0)
  const chip = transcript.getByRole('button', { name: 'exec_command', exact: true })
  await expect(chip).toHaveAttribute('aria-expanded', 'false')
  await chip.click()
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'release status',
  )
  await expect(chip).toHaveAttribute('aria-expanded', 'true')
  await expect(transcript.locator('.session-message-text, .session-tool-slot')).toHaveText([
    'Inspect the release status and retain the result.',
    /release status/,
    'The release checks passed. Publishing remains unapproved.',
  ])
  await page.screenshot({ path: testInfo.outputPath('turn-summary.png') })
})

test('persists levels and reads bounded turn detail', async ({ page }, testInfo) => {
  await turnApi(page)
  const turnReads: string[] = []
  page.on('request', (request) => {
    if (request.url().includes('/turns/')) turnReads.push(request.url())
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'passed',
  )
  await page.reload()
  await expect(page.getByRole('radio', { name: 'Tools', exact: true })).toBeChecked()
  await expect(
    transcript.getByRole('button', { name: 'Open turn details for exec_command', exact: true }),
  ).not.toHaveAttribute('aria-expanded')
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  await expect.poll(() => turnReads.length).toBeGreaterThan(0)
  await expect(
    transcript.getByText('Publishing needs an operator decision during the release window.', {
      exact: false,
    }),
  ).toBeVisible()
  await page.getByRole('radio', { name: 'Summary', exact: true }).check()
  await expect(
    transcript.getByText('Publishing needs an operator decision during the release window.', {
      exact: false,
    }),
  ).toHaveCount(0)
  await page.screenshot({ path: testInfo.outputPath('turn-levels.png') })
})

test('reads later tool members on demand in Tools mode', async ({ page }) => {
  await turnApi(page)
  const members: string[] = []
  const next = {
    type: 'more_body' as const,
    body: {
      address: { event_sequence: '2' },
      field: 'tool_arguments' as const,
      member_index: 1,
      offset_bytes: '0',
    },
  }
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('cursor_address') !== '2') return route.fallback()
    if (url.searchParams.get('cursor_member') === '1') {
      members.push('1')
      const result = toolResultItem()
      if (result.body.type !== 'tool_batch') throw new Error('Expected tool fixture')
      const argumentsText = detailExcerpt('{"cmd":"release verify"}')
      return route.fulfill({
        json: detailPage([
          {
            ...result,
            projected_body_bytes: 128 + Number(argumentsText.total_bytes),
            body: {
              ...result.body,
              projected_member_index: 1,
              tools: result.body.tools.map((tool) => ({
                ...tool,
                request_id: '00000000-0000-0000-0000-000000000125',
                tool_name: 'verify_release',
                arguments: argumentsText,
                evidence: { type: 'request_only' },
              })),
            },
          },
        ]),
      })
    }
    if (url.searchParams.get('cursor_field') === 'tool_result')
      return route.fulfill({ json: detailPage([toolResultItem()], next) })
    return route.fallback()
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'passed',
  )
  expect(members).toEqual([])
  await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
  const details = transcript.getByRole('region', { name: 'More message text' })
  await details.getByRole('button', { name: 'Continue reading', exact: true }).click()
  await expect(details.getByText('verify_release', { exact: true })).toBeVisible()
  await expect(details).toContainText('release verify')
  expect(members).toEqual(['1'])
})
