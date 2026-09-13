import { turnApi } from '../src/session-timeline/turns.fixture'
import { expect, test } from './fontTest'
import {
  detailExcerpt,
  detailItems,
  detailPage,
  detailSessionId,
  toolResultItem,
} from './session-detail-fixture'

test('shows final turn text and a tool chip while keeping lifecycle noise closed', async ({
  page,
}, testInfo) => {
  await turnApi(page)
  const outputReads: string[] = []
  page.on('request', (request) => {
    if (request.url().includes('cursor_field=tool_result')) outputReads.push(request.url())
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await expect(page.getByRole('radio', { name: 'Summary', exact: true })).toBeChecked()
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
  expect(outputReads).toEqual([])
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
  await expect(details).toContainText('passed')
  expect(members).toEqual(['1'])
})

test('preserves interleaved chronology while expanding every segment of a turn', async ({
  page,
}) => {
  const input = detailItems[0]
  if (input?.body.type !== 'user_input') throw new Error('Input fixture missing')
  const text = detailExcerpt('Start the next check after this turn.')
  const entries = [
    ...detailItems.slice(0, 2),
    {
      ...input,
      address: { event_sequence: '3' },
      projected_body_bytes: 128 + Number(text.total_bytes),
      body: {
        ...input.body,
        turn_id: '00000000-0000-0000-0000-000000000126',
        text,
        attachments: [],
      },
    },
    ...detailItems.slice(2).map((item) => ({
      ...item,
      address: { event_sequence: String(Number(item.address.event_sequence) + 1) },
    })),
  ]
  await turnApi(page, undefined, entries)
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Summary', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const messages = [
    'Inspect the release status and retain the result.',
    'Start the next check after this turn.',
    'The release checks passed. Publishing remains unapproved.',
  ]
  await expect(transcript.locator('.session-message-text')).toHaveText(messages)
  await transcript.getByRole('button', { name: 'Open turn details', exact: true }).first().click()
  await expect(transcript.getByRole('button', { name: 'Collapse turn', exact: true })).toHaveCount(
    2,
  )
  await expect(transcript.locator('.session-message-text')).toHaveText(messages)
  await transcript.getByRole('button', { name: 'Collapse turn', exact: true }).first().click()
  await expect(transcript.getByRole('button', { name: 'Collapse turn', exact: true })).toHaveCount(
    0,
  )
  await expect(transcript.locator('.session-message-text')).toHaveText(messages)
})

test('Escape collapses the focused turn before closing the product workspace', async ({ page }) => {
  await turnApi(page)
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await transcript.getByRole('button', { name: 'Open turn details', exact: true }).click()
  await transcript.getByRole('button', { name: 'Collapse turn', exact: true }).focus()
  await page.keyboard.press('Escape')
  await expect(
    transcript.getByRole('button', { name: 'Open turn details', exact: true }),
  ).toBeFocused()
  await expect(transcript).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(transcript).toHaveCount(0)
  await expect(page).not.toHaveURL(/workspace=true/)
})

test('All details retains earlier message chunks through continuation failures and retries', async ({
  page,
}) => {
  await turnApi(page)
  let failLastChunk = true
  await page.route('**/turns/*/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('cursor_address') !== '1') return route.fallback()
    const offset = url.searchParams.get('cursor_offset') ?? '0'
    if (offset === '2' && failLastChunk) return route.fulfill({ status: 503, body: '' })
    const input = detailItems[0]
    if (input?.body.type !== 'user_input') throw new Error('Input fixture missing')
    const continuation =
      offset === '2'
        ? null
        : {
            address: input.address,
            field: 'input_text' as const,
            member_index: 0,
            offset_bytes: String(Number(offset) + 1),
          }
    return route.fulfill({
      json: detailPage(
        [
          {
            ...input,
            projected_body_bytes: 129,
            body: {
              ...input.body,
              attachments: [],
              text: {
                text: offset === '0' ? 'a' : offset === '1' ? 'b' : 'c',
                offset_bytes: offset,
                total_bytes: '3',
                continuation,
              },
            },
          },
        ],
        continuation ? { type: 'more_body', body: continuation } : null,
      ),
    })
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  const input = page
    .getByRole('region', { name: 'Session transcript', exact: true })
    .locator('[data-event-sequence="1"]')
  await expect(input.locator('.session-message-text')).toHaveText(['a'])
  await input.getByRole('button', { name: 'Continue reading' }).click()
  await expect(input.locator('.session-message-text')).toHaveText(['a', 'b'])
  await input.getByRole('button', { name: 'Continue reading' }).click()
  await expect(input.getByRole('alert')).toContainText('Details could not be loaded.')
  await expect(input.locator('.session-message-text')).toHaveText(['a', 'b'])
  failLastChunk = false
  await input.getByRole('button', { name: 'Retry details' }).click()
  await expect(input.locator('.session-message-text')).toHaveText(['a', 'b', 'c'])
  await expect(input.getByRole('button', { name: 'Continue reading' })).toHaveCount(0)
})
