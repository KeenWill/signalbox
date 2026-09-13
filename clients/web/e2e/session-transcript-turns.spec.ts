import type {
  WebSessionTimelineDetail,
  WebSessionTimelineWindow,
} from '../src/generated/web-contract.mjs'
import { transcriptFixture, transcriptSessionId } from '../src/session-timeline/transcript.fixture'
import { retriedToolItems, toolGoalApi, turnApi } from '../src/session-timeline/turns.fixture'
import { expect, test } from './fontTest'
import {
  detailCallId,
  detailExcerpt,
  detailItems,
  detailLive,
  detailPage,
  detailSessionId,
  resultCursor,
  toolResultItem,
} from './session-detail-fixture'

test('shows a completed assistant response before its turn closure is loaded', async ({
  page,
}, testInfo) => {
  await turnApi(page, undefined, detailItems.slice(0, 4))
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.locator('.session-message-text')).toHaveText([
    'Inspect the release status and retain the result.',
    'The release checks passed. Publishing remains unapproved.',
  ])
  await expect(transcript.getByText('Turn completed', { exact: true })).toHaveCount(0)
  await page.screenshot({ path: testInfo.outputPath('completed-response.png') })
})

test('keeps tool calls and output reads out of Summary on desktop and phone', async ({
  page,
}, testInfo) => {
  await turnApi(page)
  const outputReads: string[] = []
  page.on('request', (request) => {
    if (request.url().includes('cursor_field=tool_result')) outputReads.push(request.url())
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const summary = page.getByRole('radio', { name: 'Summary', exact: true })
  await expect(summary).toBeChecked()
  await expect(transcript.locator('.session-message-text')).toHaveText([
    'Inspect the release status and retain the result.',
    'The release checks passed. Publishing remains unapproved.',
  ])
  await expect(transcript.getByRole('region', { name: 'Tools used' })).toHaveCount(0)
  await expect(transcript.getByText('Turn completed', { exact: true })).toHaveCount(0)
  expect(outputReads).toEqual([])
  await transcript
    .getByText('The release checks passed. Publishing remains unapproved.', { exact: true })
    .scrollIntoViewIfNeeded()
  await page.screenshot({ path: testInfo.outputPath('turn-summary.png') })
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'passed',
  )
  expect(outputReads.length).toBeGreaterThan(0)
  await summary.check()
  await expect(transcript.getByRole('region', { name: 'Tools used' })).toHaveCount(0)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.reload()
  await expect(summary).toBeChecked()
  await expect(transcript.locator('.session-message-text')).toHaveCount(2)
  await expect(transcript.getByRole('region', { name: 'Tools used' })).toHaveCount(0)
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390)
  await page.screenshot({ path: testInfo.outputPath('turn-summary-phone.png') })
})

for (const close of ['button', 'Escape']) {
  test(`hides a tool-only Summary segment and restores a surviving heading using ${close}`, async ({
    page,
  }) => {
    const input = detailItems[0]
    const tool = detailItems[1]
    const response = detailItems[3]
    if (input?.body.type !== 'user_input' || !tool || !response)
      throw new Error('Turn fixture missing')
    const other = (sequence: string) => ({
      ...input,
      address: { event_sequence: sequence },
      body: {
        ...input.body,
        turn_id: `00000000-0000-0000-0000-${sequence.padStart(12, '0')}`,
        attachments: [],
      },
    })
    await turnApi(page, undefined, [
      input,
      other('2'),
      { ...tool, address: { event_sequence: '3' } },
      other('4'),
      { ...response, address: { event_sequence: '5' } },
    ])
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    await expect(transcript.locator('[data-transcript-turn]')).toHaveCount(4)
    await expect(transcript.getByRole('region', { name: 'Tools used' })).toHaveCount(0)
    await transcript.getByRole('button', { name: 'Open turn details', exact: true }).first().click()
    const segment = transcript
      .locator('[data-transcript-turn]')
      .filter({ has: page.locator('[data-event-sequence="3"]') })
    const collapse = segment.getByRole('button', { name: 'Collapse turn', exact: true })
    if (close === 'Escape') {
      await collapse.focus()
      await page.keyboard.press('Escape')
    } else await collapse.click()
    await expect(transcript.locator('[data-transcript-turn]')).toHaveCount(4)
    await expect(transcript.locator('[data-event-sequence="3"]')).toHaveCount(0)
    await expect(
      transcript.getByRole('button', { name: 'Open turn details', exact: true }).first(),
    ).toBeFocused()
    await page.getByRole('radio', { name: 'Tools', exact: true }).check()
    await expect(transcript.locator('[data-transcript-turn]')).toHaveCount(5)
    await expect(transcript.getByRole('region', { name: 'Tools used' })).toHaveCount(1)
  })
}

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
    transcript.getByRole('button', {
      name: /^Open turn details for exec_command(?: · .+)?$/,
      exact: true,
    }),
  ).not.toHaveAttribute('aria-expanded')
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  await expect.poll(() => turnReads.length).toBeGreaterThan(0)
  await expect(
    transcript.getByRole('region', { name: 'Approval rationale', exact: true }),
  ).toBeVisible()
  await page.getByRole('radio', { name: 'Summary', exact: true }).check()
  await expect(
    transcript.getByRole('region', { name: 'Approval rationale', exact: true }),
  ).toHaveCount(0)
  await page.screenshot({ path: testInfo.outputPath('turn-levels.png') })
})

test('restores the selected Tools level after collapsing a tool-opened turn', async ({ page }) => {
  await turnApi(page)
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const tools = page.getByRole('radio', { name: 'Tools', exact: true })
  await tools.check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const summary = transcript.getByRole('region', { name: 'exec_command details', exact: true })
  const openTool = transcript.getByRole('button', {
    name: /^Open turn details for exec_command(?: · .+)?$/,
    exact: true,
  })
  const heading = transcript.getByRole('button', { name: 'Open turn details', exact: true })
  await expect(summary).toContainText('passed')
  await openTool.click()
  await transcript.getByRole('button', { name: 'Collapse turn', exact: true }).click()
  await expect(tools).toBeChecked()
  await expect(summary).toContainText('release status')
  await expect(summary).toContainText('passed')
  await expect(heading).toBeFocused()
  await openTool.click()
  await expect(transcript.getByRole('button', { name: 'Collapse turn', exact: true })).toBeFocused()
  await page.keyboard.press('Escape')
  await expect(tools).toBeChecked()
  await expect(summary).toContainText('release status')
  await expect(summary).toContainText('passed')
  await expect(heading).toBeFocused()
})

test('clears an expanded turn when a transcript level command runs', async ({ page }) => {
  await turnApi(page)
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await transcript.getByRole('button', { name: 'Open turn details', exact: true }).click()
  await expect(transcript.getByRole('button', { name: 'Collapse turn', exact: true })).toBeVisible()

  await page.getByRole('button', { name: 'Open command palette', exact: true }).click()
  await page
    .getByRole('dialog', { name: 'Command palette' })
    .getByRole('button', { name: /Show condensed transcript detail/ })
    .click()

  await expect(page.getByRole('radio', { name: 'Tools', exact: true })).toBeChecked()
  await expect(transcript.getByRole('button', { name: 'Collapse turn', exact: true })).toHaveCount(
    0,
  )
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'passed',
  )
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
  await transcript.getByRole('button', { name: 'Show more tools', exact: true }).click()
  await expect(
    transcript.getByRole('button', {
      name: /^Open turn details for verify_release(?: · .+)?$/,
      exact: true,
    }),
  ).toBeVisible()
  await expect(
    transcript.getByRole('region', { name: 'verify_release details', exact: true }),
  ).toContainText('release verify')
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

for (const level of ['Summary', 'Tools', 'All details']) {
  test(`Escape collapses the focused ${level} turn while session details stays open`, async ({
    page,
  }) => {
    await turnApi(page)
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    await page.getByRole('radio', { name: level, exact: true }).check()
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    if (level !== 'All details')
      await transcript.getByRole('button', { name: 'Open turn details', exact: true }).click()
    const details = page.getByText('Session details', { exact: true })
    const telemetry = page.getByText('Up to date as of', { exact: true })
    await details.click()
    await expect(telemetry).toBeVisible()
    await transcript.getByRole('button', { name: 'Collapse turn', exact: true }).focus()
    await page.keyboard.press('Escape')
    await expect(
      transcript.getByRole('button', { name: 'Open turn details', exact: true }),
    ).toBeFocused()
    await expect(telemetry).toBeVisible()
    await expect(page.getByRole('radio', { name: level, exact: true })).toBeChecked()
    await details.press('Escape')
    await expect(telemetry).toBeHidden()
    await expect(details).toBeFocused()
    await expect(transcript).toBeVisible()
  })
}

test('Escape collapses a focused turn while All details is selected', async ({ page }) => {
  await turnApi(page)
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const collapse = transcript.getByRole('button', { name: 'Collapse turn', exact: true })
  await collapse.focus()
  await page.keyboard.press('Escape')

  await expect(page.getByRole('radio', { name: 'All details', exact: true })).toBeChecked()
  await expect(
    transcript.getByRole('button', { name: 'Open turn details', exact: true }),
  ).toBeFocused()
  await expect(transcript).toBeVisible()
})

test('rejects changed tool evidence in a summary continuation', async ({ page }) => {
  await turnApi(page)
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('cursor_field') !== 'tool_result') return route.fallback()
    const item = toolResultItem()
    if (item.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
    const tool = item.body.tools[0]
    if (!tool || tool.evidence.type !== 'physical_attempt')
      throw new Error('Tool evidence fixture missing')
    return route.fulfill({
      json: detailPage([
        {
          ...item,
          body: {
            ...item.body,
            tools: [
              {
                ...tool,
                evidence: {
                  ...tool.evidence,
                  attempt_id: '00000000-0000-0000-0000-000000000999',
                },
              },
            ],
          },
        },
      ]),
    })
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()

  const details = page.getByRole('region', { name: 'exec_command details', exact: true })
  await expect(details.getByRole('alert')).toContainText('Output could not be loaded.')
  await expect(details).not.toContainText('passed')
})

test('renders tool-batch goal events in All details', async ({ page }) => {
  const tool = detailItems[1]
  const input = detailItems[0]
  if (!input || tool?.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
  const goalText = detailExcerpt('Release approval is required.')
  await turnApi(page, undefined, [
    input,
    {
      ...tool,
      projected_body_bytes: 128 + Number(goalText.total_bytes),
      body: {
        ...tool.body,
        tools: [],
        goal_events: [
          {
            type: 'blocked',
            generation: '1',
            reason: 'authorization_required',
            text: goalText,
          },
        ],
      },
    },
    ...detailItems.slice(2),
  ])
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'All details', exact: true }).check()

  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByText('Blocked', { exact: true })).toBeVisible()
  await expect(transcript.getByText('Authorization required', { exact: true })).toBeVisible()
  await expect(transcript.getByText('Release approval is required.', { exact: true })).toBeVisible()
})

test('All details retains earlier message chunks through continuation failures and retries', async ({
  page,
}) => {
  await turnApi(page)
  let failLastChunk = true
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if ((url.searchParams.get('cursor_address') ?? url.searchParams.get('first')) !== '1')
      return route.fallback()
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

for (const level of ['All details', 'Summary', 'Tools']) {
  test(`retains continued event text while its virtualized row is unmounted in ${level}`, async ({
    page,
  }, testInfo) => {
    const source = detailItems[0]
    if (source?.body.type !== 'user_input') throw new Error('Input fixture missing')
    const entries = Array.from({ length: 24 }, (_, index) => {
      const sequence = String(index + 1)
      const text = detailExcerpt(index === 0 ? 'first chunk' : `Message ${sequence}`)
      return {
        ...source,
        address: { event_sequence: sequence },
        projected_body_bytes: 128 + Number(text.total_bytes),
        body: {
          ...source.body,
          turn_id: `00000000-0000-0000-0000-${sequence.padStart(12, '0')}`,
          text,
          attachments: [],
        },
      }
    })
    await turnApi(page, undefined, entries)
    await page.route('**/timeline?**', (route) => {
      const url = new URL(route.request().url())
      const anchor = url.searchParams.get('anchor')
      const address = Number(url.searchParams.get('address') ?? '0')
      const maxItems = Number(url.searchParams.get('max_items') ?? '8')
      const end = anchor === 'latest' ? entries.length : Math.max(0, address - 1)
      const start = Math.max(0, end - maxItems)
      const items = entries.slice(start, end).map(({ address, kind }) => ({
        address,
        kind,
        projected_structured_bytes: 64 + kind.length,
      }))
      return route.fulfill({
        json: {
          session_id: detailSessionId,
          items,
          projected_structured_bytes: items.reduce(
            (sum, item) => sum + item.projected_structured_bytes,
            0,
          ),
          continuation_before: start > 0 ? (items[0]?.address ?? null) : null,
          continuation_after: end < entries.length ? (items.at(-1)?.address ?? null) : null,
        },
      })
    })
    await page.route('**/timeline-detail?**', (route) => {
      const url = new URL(route.request().url())
      if ((url.searchParams.get('cursor_address') ?? url.searchParams.get('first')) !== '1')
        return route.fallback()
      const offset = url.searchParams.get('cursor_offset')
      const item = entries[0]
      if (!item || item.body.type !== 'user_input') throw new Error('Input fixture missing')
      const continuation =
        offset === '23'
          ? null
          : {
              address: item.address,
              field: 'input_text' as const,
              member_index: 0,
              offset_bytes: offset === '11' ? '23' : '11',
            }
      return route.fulfill({
        json: detailPage(
          [
            {
              ...item,
              projected_body_bytes: 128 + (offset === '11' ? 12 : 11),
              body: {
                ...item.body,
                text: {
                  text:
                    offset === '23'
                      ? 'third chunk'
                      : offset === '11'
                        ? 'second chunk'
                        : 'first chunk',
                  offset_bytes: offset ?? '0',
                  total_bytes: '34',
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
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    for (const expected of ['Message 9', 'first chunk']) {
      await expect
        .poll(async () => {
          await transcript.evaluate((element) => {
            element.scrollTop = 0
            element.dispatchEvent(new Event('scroll'))
          })
          return transcript.getByText(expected, { exact: true }).isVisible()
        })
        .toBe(true)
    }
    await page.getByRole('radio', { name: level, exact: true }).check()
    const input = transcript.locator('[data-event-sequence="1"]')
    await input
      .getByRole('button', {
        name: level === 'All details' ? 'Continue reading' : 'Read more',
        exact: true,
      })
      .click()
    await input.getByRole('button', { name: 'Continue reading', exact: true }).click()
    await expect(input.locator('.session-message-text')).toHaveText([
      'first chunk',
      'second chunk',
      'third chunk',
    ])

    await transcript.focus()
    await transcript.evaluate((element) => {
      element.scrollTop = element.scrollHeight
      element.dispatchEvent(new Event('scroll'))
    })
    await expect(input).toHaveCount(0)
    await transcript.evaluate((element) => {
      element.scrollTop = 0
      element.dispatchEvent(new Event('scroll'))
    })
    await expect(input.locator('.session-message-text')).toHaveText([
      'first chunk',
      'second chunk',
      'third chunk',
    ])
    if (level !== 'All details') {
      await input.getByRole('button', { name: 'Close details', exact: true }).click()
      await expect(input.locator('.session-message-text')).toHaveText(['first chunk'])
      await input.getByRole('button', { name: 'Read more', exact: true }).click()
      await expect(input.locator('.session-message-text')).toHaveText([
        'first chunk',
        'second chunk',
      ])
    }
    await page.screenshot({ path: testInfo.outputPath('retained-continuation.png') })
  })
}

test('preserves assistant text before its tool without treating it as the final response', async ({
  page,
}) => {
  const input = detailItems[0]
  const tool = detailItems[1]
  const response = detailItems[3]
  const completion = detailItems[4]
  if (!input || !tool || response?.body.type !== 'model_call' || !completion)
    throw new Error('Turn fixture missing')
  const text = detailExcerpt('I will inspect the release status now.')
  const intermediate = {
    ...response,
    address: { event_sequence: '2' },
    projected_body_bytes: 128 + Number(text.total_bytes),
    body: { ...response.body, model_call_id: detailCallId, response: text },
  }
  await turnApi(page, undefined, [
    input,
    intermediate,
    { ...tool, address: { event_sequence: '3' } },
    response,
    completion,
  ])
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.locator('.session-message-text')).toHaveText([
    'Inspect the release status and retain the result.',
    'I will inspect the release status now.',
    'The release checks passed. Publishing remains unapproved.',
  ])
  await expect(transcript.locator('.session-message-text, .session-tool-chips')).toHaveText([
    'Inspect the release status and retain the result.',
    'I will inspect the release status now.',
    'The release checks passed. Publishing remains unapproved.',
  ])
})

for (const recovered of [false, true]) {
  test(`retains a provider failure ${recovered ? 'before its successful retry' : 'without a lifecycle event'}`, async ({
    page,
  }) => {
    const model = detailItems[3]
    const completion = detailItems[4]
    if (model?.body.type !== 'model_call' || !completion) throw new Error('Model fixture missing')
    const failure = {
      ...model,
      address: { event_sequence: '1' },
      projected_body_bytes: 128,
      body: {
        ...model.body,
        response: null,
        provider_failure_cause: 'quota_exhausted' as const,
        state: { type: 'terminal' as const, disposition: 'known_failed' as const },
      },
    }
    await turnApi(page, undefined, [failure, ...(recovered ? [model, completion] : [])])
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    await expect(transcript.getByText('Provider error: Quota reached')).toBeVisible()
    await expect(transcript.locator('.session-turn-outcome, .session-message-text')).toHaveText([
      'Provider error: Quota reached',
      ...(recovered ? ['The release checks passed. Publishing remains unapproved.'] : []),
    ])
  })
}

for (const type of ['blocked', 'achieved'] as const) {
  test(`All details renders a tool-produced ${type} goal and continues its text`, async ({
    page,
  }) => {
    const { prefix, suffix } = await toolGoalApi(page, type)
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    await page.getByRole('radio', { name: 'All details', exact: true }).check()
    const event = page
      .getByRole('region', { name: 'Session transcript', exact: true })
      .locator('.session-turn-event[data-event-sequence="2"]')
    const next = event.getByRole('button', { name: 'Continue reading', exact: true })
    await next.click()
    await next.click()
    const goals = event.getByRole('region', { name: 'Goal events', exact: true })
    await expect(goals).toContainText(type === 'blocked' ? 'Blocked' : 'Achieved')
    if (type === 'blocked') await expect(goals).toContainText('Input required')
    await expect(
      goals.getByRole('region', { name: 'Goal text', exact: true }).locator('pre'),
    ).toHaveText(prefix)
    await next.click()
    await expect(
      goals.getByRole('region', { name: 'Goal text', exact: true }).locator('pre'),
    ).toHaveText([prefix, suffix])
    await expect(next).toHaveCount(0)
  })
}

test('keeps a failed physical attempt inspectable after the same request succeeds', async ({
  page,
}, testInfo) => {
  const items = retriedToolItems()
  await turnApi(page, undefined, items)
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    const item = items.find((item) => item.address.event_sequence === url.searchParams.get('first'))
    if (item?.body.type !== 'tool_batch')
      return item ? route.fulfill({ json: detailPage([item]) }) : route.fallback()
    const tool = item.body.tools[0]
    if (!tool) throw new Error('Tool fixture missing')
    const physical = tool.evidence.type === 'physical_attempt' ? tool.evidence : null
    const field = physical?.failure_present ? 'tool_failure' : 'tool_result'
    const continued = url.searchParams.has('cursor_field')
    const excerpt = continued ? (physical?.failure ?? physical?.result) : tool.arguments
    const projected = {
      ...item,
      projected_body_bytes: 128 + Number(excerpt?.total_bytes ?? '0'),
      body: {
        ...item.body,
        tools: [
          {
            ...tool,
            arguments: continued ? null : tool.arguments,
            evidence: physical
              ? {
                  ...physical,
                  result: continued ? physical.result : null,
                  failure: continued ? physical.failure : null,
                }
              : tool.evidence,
          },
        ],
      },
    }
    return route.fulfill({
      json: detailPage(
        [projected],
        physical && !continued
          ? {
              type: 'more_body',
              body: { address: item.address, field, member_index: 0, offset_bytes: '0' },
            }
          : null,
      ),
    })
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const chips = transcript.getByRole('button', {
    name: /^Open turn details for exec_command(?: · .+)?$/,
    exact: true,
  })
  await expect(chips).toHaveCount(2)
  await expect(chips.first()).toHaveText('exec_command · Failed')
  await expect(chips.first()).toHaveAccessibleName('Open turn details for exec_command · Failed')
  await expect(chips.last()).toHaveText('exec_command · Completed')
  await expect(chips.last()).toHaveAccessibleName('Open turn details for exec_command · Completed')
  const slots = transcript.locator('.session-tool-slot')
  await expect(slots.first().getByText('Failure · Attempt lost on restart')).toBeVisible()
  await slots.first().getByRole('button', { name: 'Read more', exact: true }).click()
  await slots.last().getByRole('button', { name: 'Read more', exact: true }).click()
  await expect(
    slots
      .first()
      .getByRole('region', { name: 'More message text', exact: true })
      .getByRole('region', { name: 'Failure', exact: true }),
  ).toContainText('Runner disconnected during the release check.')
  await expect(
    slots
      .last()
      .getByRole('region', { name: 'More message text', exact: true })
      .getByRole('region', { name: 'Output', exact: true }),
  ).toContainText('passed')
  await expect(transcript.locator('.session-message-text, .session-tool-slot')).toHaveText([
    'Inspect the release status and retain the result.',
    /Runner disconnected/,
    'Check the next release too.',
    /checks.*passed/,
    'The release checks passed. Publishing remains unapproved.',
  ])
  await page.screenshot({ path: testInfo.outputPath('retried-tool.png') })
})

for (const level of ['Tools']) {
  test(`retains an opened tool continuation across virtual unmounts in ${level}`, async ({
    page,
  }) => {
    const input = detailItems[0]
    const original = detailItems[1]
    if (input?.body.type !== 'user_input' || original?.body.type !== 'tool_batch')
      throw new Error('Fixture missing')
    const tool = original.body.tools[0]
    if (tool?.evidence.type !== 'physical_attempt') throw new Error('Attempt missing')
    const argumentsText = detailExcerpt('first chunk')
    const first = {
      ...original,
      address: { event_sequence: '1' },
      projected_body_bytes: 128 + Number(argumentsText.total_bytes),
      body: { ...original.body, tools: [{ ...tool, arguments: argumentsText }] },
    }
    const entries = Array.from({ length: 24 }, (_, index) => {
      if (index === 0) return first
      const sequence = String(index + 1)
      const text = detailExcerpt(`Message ${sequence}`)
      return {
        ...input,
        address: { event_sequence: sequence },
        projected_body_bytes: 128 + Number(text.total_bytes),
        body: {
          ...input.body,
          turn_id: `00000000-0000-0000-0000-${sequence.padStart(12, '0')}`,
          text,
          attachments: [],
        },
      }
    })
    await turnApi(page, undefined, entries)
    await page.route('**/timeline?**', (route) => {
      const window = transcriptFixture(
        new URL(route.request().url()),
        24,
      ) as WebSessionTimelineWindow
      const items = window.items.map((item) => {
        const kind = item.address.event_sequence === '1' ? first.kind : item.kind
        return { ...item, kind, projected_structured_bytes: 64 + kind.length }
      })
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
    })
    await page.route('**/timeline-detail?**', (route) => {
      const url = new URL(route.request().url())
      if ((url.searchParams.get('cursor_address') ?? url.searchParams.get('first')) !== '1')
        return route.fallback()
      const output = url.searchParams.get('cursor_field') === 'tool_result'
      const last = url.searchParams.get('cursor_offset') === '12'
      const text = last ? 'third chunk' : 'second chunk'
      const continuation = {
        address: first.address,
        field: 'tool_result' as const,
        member_index: 0,
        offset_bytes: output ? '12' : '0',
      }
      const item = output
        ? {
            ...first,
            projected_body_bytes: 128 + text.length,
            body: {
              ...first.body,
              tools: [
                {
                  ...tool,
                  arguments: null,
                  evidence: {
                    ...tool.evidence,
                    result: {
                      text,
                      offset_bytes: last ? '12' : '0',
                      total_bytes: '23',
                      continuation: last ? null : continuation,
                    },
                  },
                },
              ],
            },
          }
        : first
      return route.fulfill({
        json: detailPage([item], last ? null : { type: 'more_body', body: continuation }),
      })
    })
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    await page.getByRole('radio', { name: level, exact: true }).check()
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    for (const name of ['Message 9', 'exec_command']) {
      await expect
        .poll(async () => {
          await transcript.evaluate((element) => {
            element.scrollTop = 0
            element.dispatchEvent(new Event('scroll'))
          })
          return transcript.getByText(name, { exact: true }).first().isVisible()
        })
        .toBe(true)
    }
    await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
    const reader = transcript.getByRole('region', { name: 'More message text', exact: true })
    await reader.getByRole('button', { name: 'Continue reading', exact: true }).click()
    await expect(reader.locator('pre')).toHaveText(['second chunk', 'third chunk'])
    await transcript.evaluate((element) => {
      element.scrollTop = element.scrollHeight
      element.dispatchEvent(new Event('scroll'))
    })
    await expect(reader).toHaveCount(0)
    await transcript.evaluate((element) => {
      element.scrollTop = 0
      element.dispatchEvent(new Event('scroll'))
    })
    await expect(reader.locator('pre')).toHaveText(['second chunk', 'third chunk'])
    await reader.getByRole('button', { name: 'Close details', exact: true }).click()
    await expect(reader).toHaveCount(0)
    await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
    await expect(reader.locator('pre')).toHaveText(['second chunk'])
  })
}

for (const level of ['Tools']) {
  test(`reads interleaved tool output from its terminal event in ${level}`, async ({
    page,
  }, testInfo) => {
    const input = detailItems[0]
    const tool = detailItems[1]
    if (input?.body.type !== 'user_input' || tool?.body.type !== 'tool_batch')
      throw new Error('Fixture missing')
    const proposal = {
      ...tool,
      body: {
        ...tool.body,
        tools: tool.body.tools.map((tool) => ({
          ...tool,
          evidence: { type: 'request_only' as const },
        })),
      },
    }
    const entries = [
      input,
      proposal,
      {
        ...input,
        address: { event_sequence: '3' },
        body: { ...input.body, turn_id: '00000000-0000-0000-0000-000000000126' },
      },
      { ...tool, address: { event_sequence: '4' } },
    ]
    await turnApi(page, undefined, entries)
    const addresses: string[] = []
    await page.route('**/timeline-detail?**', (route) => {
      const url = new URL(route.request().url())
      if (url.searchParams.get('first') === '2' && !url.searchParams.has('cursor_field'))
        return route.fulfill({ json: detailPage([proposal]) })
      if (url.searchParams.get('cursor_field') !== 'tool_result') return route.fallback()
      const address = url.searchParams.get('cursor_address') ?? ''
      addresses.push(address)
      return address === '4' ? route.fallback() : route.fulfill({ status: 400, body: '' })
    })
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    await page.getByRole('radio', { name: level, exact: true }).check()
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    const summary = transcript.getByRole('region', { name: 'exec_command details', exact: true })
    await expect(summary.getByRole('region', { name: 'Output', exact: true })).toContainText(
      'passed',
    )
    expect(addresses).toEqual(['4'])
    await page.screenshot({ path: testInfo.outputPath('terminal-tool-output.png') })
  })
}

for (const changed of ['attempt', 'attachments', 'total', 'excerpt contents']) {
  test(`rejects an initial expanded event reread with changed ${changed}`, async ({ page }) => {
    await turnApi(page)
    let corrupt = true
    const sequence = changed === 'attempt' ? '2' : '1'
    await page.route('**/turns/*/timeline-detail?**', (route) => {
      const url = new URL(route.request().url())
      if (!corrupt || url.searchParams.get('cursor_address') !== sequence) return route.fallback()
      const input = detailItems[0]
      const batch = detailItems[1]
      if (input?.body.type !== 'user_input' || batch?.body.type !== 'tool_batch')
        throw new Error('Fixture missing')
      const text = detailExcerpt('Contradictory expanded message.')
      const item =
        changed === 'attempt'
          ? {
              ...batch,
              body: {
                ...batch.body,
                tools: batch.body.tools.map((tool) => ({
                  ...tool,
                  evidence:
                    tool.evidence.type === 'physical_attempt'
                      ? { ...tool.evidence, attempt_id: '00000000-0000-0000-0000-000000000999' }
                      : tool.evidence,
                })),
              },
            }
          : {
              ...input,
              projected_body_bytes:
                changed === 'total' ? 128 + Number(text.total_bytes) : input.projected_body_bytes,
              body: {
                ...input.body,
                attachments: changed === 'attachments' ? input.body.attachments : [],
                text:
                  changed === 'total'
                    ? text
                    : changed === 'excerpt contents'
                      ? { ...input.body.text, text: `!${input.body.text.text.slice(1)}` }
                      : input.body.text,
              },
            }
      return route.fulfill({
        json: detailPage(
          [item],
          changed === 'attempt'
            ? {
                type: 'more_body',
                body: {
                  address: batch.address,
                  field: 'tool_result',
                  member_index: 0,
                  offset_bytes: '0',
                },
              }
            : null,
        ),
      })
    })
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    await page.getByRole('radio', { name: 'All details', exact: true }).check()
    const event = page
      .getByRole('region', { name: 'Session transcript', exact: true })
      .locator(`[data-event-sequence="${sequence}"]`)
    await expect(event.getByRole('alert')).toContainText('Details could not be loaded.')
    await expect(event.locator('.session-message-text, .session-tool-entry')).toHaveCount(0)
    corrupt = false
    await event.getByRole('button', { name: 'Retry details', exact: true }).click()
    await expect(event.getByRole('alert')).toHaveCount(0)
    await expect(event).toContainText(
      changed === 'attempt' ? 'release status' : 'Inspect the release status',
    )
  })
}

test('accepts a longer expanded excerpt with the same retained immutable facts', async ({
  page,
}) => {
  await turnApi(page)
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.includes('/turns/') || url.searchParams.get('first') !== '1')
      return route.fallback()
    const input = detailItems[0]
    if (input?.body.type !== 'user_input') throw new Error('Fixture missing')
    const continuation = {
      address: input.address,
      field: 'input_text' as const,
      member_index: 0,
      offset_bytes: '7',
    }
    return route.fulfill({
      json: detailPage(
        [
          {
            ...input,
            projected_body_bytes: 135,
            body: {
              ...input.body,
              attachments: [],
              text: { ...input.body.text, text: 'Inspect', continuation },
            },
          },
        ],
        { type: 'more_body', body: continuation },
      ),
    })
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByText('Inspect', { exact: true })).toBeVisible()
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  await expect(
    transcript.getByText('Inspect the release status and retain the result.', { exact: true }),
  ).toBeVisible()
  await expect(transcript.getByRole('alert')).toHaveCount(0)
})

test('keeps the retained tool in its original row when its earlier proposal is prepended', async ({
  page,
}, testInfo) => {
  const input = detailItems[0]
  const tool = detailItems[1]
  if (input?.body.type !== 'user_input' || tool?.body.type !== 'tool_batch')
    throw new Error('Fixture missing')
  const batch = tool.body
  const entries = Array.from({ length: 16 }, (_, index) => {
    const address = { event_sequence: String(index + 1) }
    if (index === 8) return { ...tool, address }
    if (index === 1)
      return {
        ...tool,
        address,
        body: {
          ...batch,
          tools: batch.tools.map((entry) => ({
            ...entry,
            evidence: { type: 'request_only' as const },
          })),
        },
      }
    const text = detailExcerpt(`Message ${index + 1}`)
    return {
      ...input,
      address,
      projected_body_bytes: 128 + Number(text.total_bytes),
      body: { ...input.body, text, attachments: [] },
    }
  })
  await turnApi(page, undefined, entries)
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('first') !== '2') return route.fallback()
    const proposal = entries[1]
    if (!proposal) throw new Error('Proposal fixture missing')
    return route.fulfill({ json: detailPage([proposal]) })
  })
  let release = () => {}
  const olderReady = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/timeline?**', async (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('anchor') === 'before') await olderReady
    const window = transcriptFixture(url, 16) as WebSessionTimelineWindow
    const items = window.items.map((item) => {
      const kind = ['2', '9'].includes(item.address.event_sequence) ? tool.kind : item.kind
      return { ...item, kind, projected_structured_bytes: 64 + kind.length }
    })
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
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.locator('[data-turn-id]')).toHaveCount(1)
  const row = await transcript.locator('[data-turn-id]').first().elementHandle()
  if (!row) throw new Error('Retained row missing')
  await transcript.evaluate((element) => {
    element.scrollTop = 0
    element.dispatchEvent(new Event('scroll'))
  })
  release()
  await expect(transcript.locator('[data-turn-id]')).toHaveCount(2)
  expect(await row.evaluate((element) => element.isConnected)).toBe(true)
  await expect(
    transcript.getByRole('region', { name: 'exec_command details', exact: true }),
  ).toContainText('release status')
  await page.screenshot({ path: testInfo.outputPath('retained-tool-segment.png') })
})

for (const around of ['18446744073709551616', '99999999999999999999']) {
  test(`ignores an out-of-range around address ${around}`, async ({ page }) => {
    await turnApi(page)
    const anchors: string[] = []
    page.on('request', (request) => {
      const url = new URL(request.url())
      if (url.pathname.endsWith('/timeline')) anchors.push(url.searchParams.get('anchor') ?? '')
    })
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}&around=${around}`)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    await expect(
      transcript.getByText('The release checks passed. Publishing remains unapproved.', {
        exact: true,
      }),
    ).toBeVisible()
    await expect(page.getByRole('alert')).toHaveCount(0)
    expect(anchors.length).toBeGreaterThan(0)
    expect(anchors.every((anchor) => anchor === 'latest')).toBe(true)
  })
}

for (const open of ['Summary', 'Tools', 'turn link']) {
  test(`includes a turn-associated compaction without turn_id from ${open}`, async ({
    page,
  }, testInfo) => {
    const input = detailItems[0]
    const response = detailItems[3]
    const completion = detailItems[4]
    if (input?.body.type !== 'user_input' || !response || !completion)
      throw new Error('Fixture missing')
    const turnId = input.body.turn_id
    const summary = detailExcerpt('Earlier conversation condensed for this turn.')
    const compaction: WebSessionTimelineDetail = {
      address: { event_sequence: '2' },
      kind: 'context_compacted',
      projected_body_bytes: 128 + Number(summary.total_bytes),
      body: {
        type: 'context_compaction',
        compaction_id: '00000000-0000-0000-0000-000000000131',
        model_call_id: detailCallId,
        result_frontier_id: '00000000-0000-0000-0000-000000000132',
        summary_entry_id: '00000000-0000-0000-0000-000000000133',
        through_position: '1',
        summary,
      },
    }
    const unrelated: WebSessionTimelineDetail = {
      address: { event_sequence: '3' },
      kind: 'goal_turn_retired',
      projected_body_bytes: 128,
      body: { type: 'event_fact', kind: 'goal_turn_retired' },
    }
    const entries = [input, compaction, unrelated, response, completion]
    await turnApi(page, undefined, entries)
    const membership: string[] = []
    let unavailable = open === 'Summary'
    await page.route('**/turns/*/timeline-detail?**', (route) => {
      const url = new URL(route.request().url())
      const address = url.searchParams.get('cursor_address') ?? '1'
      if (address === '2' || address === '3') membership.push(address)
      if (address === '2' && unavailable)
        return route.fulfill({
          status: 503,
          json: {
            error: {
              code: 'session_projection_unavailable',
              kind: 'application',
              message: 'Turn detail is temporarily unavailable.',
            },
          },
        })
      if (address === '3')
        return route.fulfill({
          status: 400,
          json: {
            error: {
              code: 'invalid_timeline_detail_limits',
              kind: 'transport',
              message: 'Address is not part of this turn.',
            },
          },
        })
      const item = entries.find(
        (item) => item !== unrelated && BigInt(item.address.event_sequence) >= BigInt(address),
      )
      if (!item) return route.fulfill({ json: detailPage([]) })
      const loaded =
        item.body.type === 'user_input'
          ? { ...item, body: { ...item.body, attachments: [] } }
          : item
      return route.fulfill({ json: detailPage([loaded]) })
    })
    await page.goto(
      `/sessions?workspace=true&session=${detailSessionId}${open === 'turn link' ? `&turn=${turnId}` : ''}`,
    )
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    if (open !== 'turn link') {
      await page.getByRole('radio', { name: open, exact: true }).check()
      await transcript
        .getByRole('button', { name: 'Open turn details', exact: true })
        .first()
        .click()
    }
    const event = transcript.locator('[data-event-sequence="2"]')
    if (unavailable) {
      await expect(page.getByRole('alert')).toContainText('Turn details could not be loaded.')
      await expect(event).toHaveCount(0)
      unavailable = false
      await page.getByRole('button', { name: 'Retry turn details', exact: true }).click()
    }
    await expect(event).toContainText(summary.text)
    await expect(page.getByRole('alert')).toHaveCount(0)
    await expect(event).toBeVisible()
    await expect(
      transcript
        .locator('[data-turn-id]')
        .filter({ has: page.locator('[data-event-sequence="2"]') }),
    ).toHaveAttribute('data-turn-id', turnId)
    await expect(transcript.locator('[data-event-sequence="3"]')).toHaveCount(0)
    expect(membership.filter((address) => address === '3')).toHaveLength(1)
    expect(membership.filter((address) => address === '2').length).toBeLessThanOrEqual(
      open === 'Summary' ? 3 : 2,
    )
    await page.screenshot({ path: testInfo.outputPath('associated-compaction.png') })
    const collapse = transcript
      .locator('[data-transcript-turn]')
      .filter({ has: page.locator('[data-event-sequence="2"]') })
      .getByRole('button', { name: 'Collapse turn', exact: true })
    if (open === 'Tools') {
      await collapse.focus()
      await page.keyboard.press('Escape')
    } else await collapse.click()
    await expect(event).toHaveCount(0)
    await expect(
      transcript.getByRole('button', { name: 'Open turn details', exact: true }).first(),
    ).toBeFocused()
  })
}

for (const afterOutput of [false, true]) {
  test(`shows later batch members as separate chips${afterOutput ? ' after reading tool output' : ''}`, async ({
    page,
  }, testInfo) => {
    const original = detailItems[1]
    if (original?.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
    const batch = original.body
    const tool = batch.tools[0]
    if (!tool) throw new Error('Tool member missing')
    const cursor = (
      member: number,
      field: 'tool_arguments' | 'tool_result' = 'tool_arguments',
    ) => ({
      type: 'more_body' as const,
      body: { address: original.address, field, member_index: member, offset_bytes: '0' },
    })
    const member = (index: number): WebSessionTimelineDetail => ({
      ...original,
      projected_body_bytes: 128 + Number(detailExcerpt(`Arguments for tool ${index}`).total_bytes),
      body: {
        ...batch,
        projected_member_index: index,
        tools: [
          {
            ...tool,
            request_id: `00000000-0000-0000-0000-${String(140 + index).padStart(12, '0')}`,
            tool_name: ['exec_command', 'read_file', 'apply_patch'][index] ?? 'tool',
            arguments: detailExcerpt(`Arguments for tool ${index}`),
            evidence: index === 0 && afterOutput ? tool.evidence : { type: 'request_only' },
          },
        ],
      },
    })
    const first = member(0)
    await turnApi(
      page,
      undefined,
      detailItems.map((item) => (item === original ? first : item)),
    )
    const reads: number[] = []
    let unavailable = true
    await page.route('**/timeline-detail?**', (route) => {
      const url = new URL(route.request().url())
      if ((url.searchParams.get('cursor_address') ?? url.searchParams.get('first')) !== '2')
        return route.fallback()
      const index = Number(url.searchParams.get('cursor_member') ?? '0')
      if (url.searchParams.has('cursor_member')) reads.push(index)
      if (index === 1 && unavailable)
        return route.fulfill({
          status: 503,
          json: {
            error: { kind: 'application', code: 'timeline_unavailable', message: 'Try again.' },
          },
        })
      if (url.searchParams.get('cursor_field') === 'tool_result') {
        if (first.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
        const firstTool = first.body.tools[0]
        if (firstTool?.evidence.type !== 'physical_attempt')
          throw new Error('Attempt fixture missing')
        const result = detailExcerpt('First tool output')
        return route.fulfill({
          json: detailPage(
            [
              {
                ...first,
                projected_body_bytes: 128 + Number(result.total_bytes),
                body: {
                  ...first.body,
                  tools: [
                    { ...firstTool, arguments: null, evidence: { ...firstTool.evidence, result } },
                  ],
                },
              },
            ],
            cursor(1),
          ),
        })
      }
      return route.fulfill({
        json: detailPage(
          [member(index)],
          index === 0 && afterOutput
            ? cursor(0, 'tool_result')
            : index < 2
              ? cursor(index + 1)
              : null,
        ),
      })
    })
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    await page.getByRole('radio', { name: 'Tools', exact: true }).check()
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    const chips = transcript.getByRole('region', { name: 'Tools used' })
    await expect(
      chips.getByRole('button', {
        name: /^Open turn details for exec_command(?: · .+)?$/,
        exact: true,
      }),
    ).toBeVisible()
    if (afterOutput) {
      await expect(
        chips.getByRole('region', { name: 'exec_command details', exact: true }),
      ).toContainText('First tool output')
      expect(reads).toEqual([0])
      await chips.getByRole('button', { name: 'Read more', exact: true }).click()
      await expect(
        chips
          .getByRole('region', { name: 'More message text', exact: true })
          .getByText('First tool output', { exact: true }),
      ).toBeVisible()
    }
    await chips.getByRole('button', { name: 'Show more tools', exact: true }).click()
    await expect(chips.getByRole('alert')).toHaveText('More tools could not be loaded.')
    unavailable = false
    await chips.getByRole('button', { name: 'Retry more tools', exact: true }).click()
    const second = chips.getByRole('button', {
      name: /^Open turn details for read_file(?: · .+)?$/,
      exact: true,
    })
    await expect(second).toBeVisible()
    await expect(
      chips.getByRole('region', { name: 'read_file details', exact: true }),
    ).toContainText('Arguments for tool 1')
    await chips.getByRole('button', { name: 'Show more tools', exact: true }).click()
    const third = chips.getByRole('button', {
      name: /^Open turn details for apply_patch(?: · .+)?$/,
      exact: true,
    })
    await expect(third).toBeVisible()
    await expect(
      chips.getByRole('region', { name: 'apply_patch details', exact: true }),
    ).toContainText('Arguments for tool 2')
    await expect(chips.getByRole('button', { name: 'Show more tools', exact: true })).toHaveCount(0)
    expect(reads).toEqual(afterOutput ? [0, 0, 1, 1, 2] : [1, 1, 2])
    await page.screenshot({ path: testInfo.outputPath('separate-batch-chips.png') })
    await transcript.getByRole('button', { name: 'Open turn details', exact: true }).first().click()
    await expect(transcript.locator('[data-event-sequence="2"]')).toBeVisible()
    await expect(transcript.getByRole('alert')).toHaveCount(0)
  })
}

async function continuedMessageApi(page: import('./fontTest').Page, interleaved = false) {
  const input = detailItems[0]
  const response = detailItems[3]
  if (input?.body.type !== 'user_input' || !response) throw new Error('Message fixture missing')
  const state = { failContinuation: false }
  const chunk = (offset: string) => {
    const continuation =
      offset === '2'
        ? null
        : {
            address: input.address,
            field: 'input_text' as const,
            member_index: 0,
            offset_bytes: String(Number(offset) + 1),
          }
    return {
      item: {
        ...input,
        projected_body_bytes: 129,
        body: {
          ...input.body,
          text: {
            text: offset === '0' ? 'a' : offset === '1' ? 'b' : 'c',
            offset_bytes: offset,
            total_bytes: '3',
            continuation,
          },
        },
      },
      continuation,
    }
  }
  const first = chunk('0').item
  const otherText = detailExcerpt('An interleaved turn.')
  const entries = interleaved
    ? [
        first,
        {
          ...input,
          address: { event_sequence: '2' },
          projected_body_bytes: 128 + Number(otherText.total_bytes),
          body: {
            ...input.body,
            turn_id: '00000000-0000-0000-0000-000000000126',
            text: otherText,
            attachments: [],
          },
        },
        { ...response, address: { event_sequence: '3' } },
      ]
    : [first]
  await turnApi(page, undefined, entries)
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if ((url.searchParams.get('cursor_address') ?? url.searchParams.get('first')) !== '1')
      return route.fallback()
    const offset = url.searchParams.get('cursor_offset') ?? '0'
    if (offset !== '0' && state.failContinuation) return route.fulfill({ status: 503, body: '' })
    const { item, continuation } = chunk(offset)
    return route.fulfill({
      json: detailPage([item], continuation ? { type: 'more_body', body: continuation } : null),
    })
  })
  return state
}

for (const level of ['Summary', 'Tools', 'All details']) {
  test(`renders message attachments once across continued chunks in ${level}`, async ({
    page,
  }, testInfo) => {
    await continuedMessageApi(page)
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    await page.getByRole('radio', { name: level, exact: true }).check()
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    await transcript
      .getByRole('button', {
        name: level === 'All details' ? 'Continue reading' : 'Read more',
        exact: true,
      })
      .click()
    await transcript.getByRole('button', { name: 'Continue reading', exact: true }).click()
    await expect(transcript.locator('.session-message-text')).toHaveText(['a', 'b', 'c'])
    await expect(transcript.getByRole('list', { name: 'Attachments' })).toHaveCount(1)
    await expect(transcript.getByRole('listitem')).toHaveCount(2)
    await page.screenshot({ path: testInfo.outputPath('continued-message-attachments.png') })
    if (level === 'Summary') {
      await page.setViewportSize({ width: 390, height: 844 })
      await page.screenshot({
        path: testInfo.outputPath('continued-message-attachments-phone.png'),
        fullPage: true,
      })
    }
  })
}

for (const close of ['button', 'Escape']) {
  test(`clears readers across all interleaved turn segments using ${close}`, async ({ page }) => {
    await continuedMessageApi(page, true)
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    const message = transcript.locator('[data-event-sequence="1"]')
    await message.getByRole('button', { name: 'Read more', exact: true }).click()
    await message.getByRole('button', { name: 'Continue reading', exact: true }).click()
    await expect(message.locator('.session-message-text')).toHaveText(['a', 'b', 'c'])
    await transcript.getByRole('button', { name: 'Open turn details', exact: true }).last().click()
    await expect(
      transcript.getByRole('button', { name: 'Collapse turn', exact: true }),
    ).toHaveCount(2)
    // Read chunks in the other segment while the complete turn is expanded.
    await message.getByRole('button', { name: 'Continue reading', exact: true }).click()
    await message.getByRole('button', { name: 'Continue reading', exact: true }).click()
    await expect(message.locator('.session-message-text')).toHaveText(['a', 'b', 'c'])
    const collapse = transcript.getByRole('button', { name: 'Collapse turn', exact: true }).last()
    if (close === 'Escape') {
      await collapse.focus()
      await page.keyboard.press('Escape')
    } else await collapse.click()
    await expect(transcript.getByRole('region', { name: 'More message text' })).toHaveCount(0)
    await expect(message.locator('.session-message-text')).toHaveText(['a'])
    await message.getByRole('button', { name: 'Read more', exact: true }).click()
    await expect(message.locator('.session-message-text')).toHaveText(['a', 'b'])
    await transcript.getByRole('button', { name: 'Open turn details', exact: true }).last().click()
    await expect(message.locator('.session-message-text')).toHaveText(['a'])
  })
}

for (const level of ['Summary', 'Tools']) {
  test(`releases the closed continuation query before reopening in ${level}`, async ({ page }) => {
    const state = await continuedMessageApi(page)
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    await page.getByRole('radio', { name: level, exact: true }).check()
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
    await expect(transcript.locator('.session-message-text')).toHaveText(['a', 'b'])
    await transcript.getByRole('button', { name: 'Close details', exact: true }).click()
    state.failContinuation = true
    await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
    await expect(transcript.getByRole('alert')).toContainText('Details could not be loaded.')
    await expect(transcript.locator('.session-message-text')).toHaveText(['a'])
  })
}

test('releases the Tools reader after stopping before a goal member', async ({ page }) => {
  const { prefix, suffix } = await toolGoalApi(page, 'blocked')
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
  const reader = transcript.getByRole('region', { name: 'More message text', exact: true })
  await expect(reader).toContainText('passed')
  await expect(reader.getByRole('button', { name: 'Continue reading', exact: true })).toHaveCount(0)
  await expect(reader).not.toContainText(prefix)
  await expect(reader).not.toContainText(suffix)
  await reader.getByRole('button', { name: 'Close details', exact: true }).click()
  await expect(reader).toHaveCount(0)
  await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
  await expect(reader).toContainText('passed')
  await expect(reader).not.toContainText(prefix)
  await expect(reader).not.toContainText(suffix)
})

test('automatically scans past goal-only batch windows to earlier conversation', async ({
  page,
}) => {
  const reads: string[] = []
  const goal = detailExcerpt('Earlier goal outcome')
  const batch = detailItems[1]
  if (batch?.body.type !== 'tool_batch') throw new Error('Batch fixture missing')
  await page.route('**/api/**', (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/follow'))
      return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
    if (url.pathname === '/api/attention')
      return route.fulfill({
        json: { cursor: '0', summaries: [], continuation_after_session_id: null },
      })
    const payload = transcriptFixture(url)
    if (url.pathname.endsWith('/timeline')) {
      if (url.searchParams.get('max_items') === '8')
        reads.push(url.searchParams.get('anchor') ?? '')
      const window = payload as WebSessionTimelineWindow
      const items = window.items.map((item) => {
        const kind = Number(item.address.event_sequence) > 99984 ? batch.kind : item.kind
        return { ...item, kind, projected_structured_bytes: 64 + kind.length }
      })
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
    if (
      url.pathname.endsWith('/timeline-detail') &&
      Number(url.searchParams.get('first')) > 99984
    ) {
      const item = {
        ...batch,
        address: { event_sequence: url.searchParams.get('first') ?? '' },
        projected_body_bytes: 128 + Number(goal.total_bytes),
        body: {
          ...batch.body,
          tools: [],
          goal_events: [{ type: 'achieved' as const, generation: '1', text: goal }],
        },
      }
      const detail = detailPage([item])
      return route.fulfill({ json: { ...detail, session_id: transcriptSessionId } })
    }
    return route.fulfill({ json: payload })
  })
  await page.goto(`/sessions?workspace=true&session=${transcriptSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await expect(transcript.getByText('Message 99984', { exact: true })).toBeVisible()
  expect(reads).toEqual(['latest', 'before', 'before'])
  await expect(transcript.getByRole('region', { name: 'Tools used' })).toHaveCount(0)
  expect(Number(await transcript.getAttribute('data-total-loaded'))).toBeLessThanOrEqual(24)
})

test('keeps an open request disclosure and its continued text when physical attempts arrive', async ({
  page,
}, testInfo) => {
  const problems: string[] = []
  page.on('pageerror', (error) => problems.push(error.message))
  page.on('console', (message) => {
    if (message.type() === 'error') problems.push(message.text())
  })
  const attempts = retriedToolItems()
  const pending = attempts[1]
  const first = attempts[2]
  const retry = attempts[4]
  if (pending?.body.type !== 'tool_batch' || !first || !retry)
    throw new Error('Attempt fixture missing')
  const text = '{"cmd":"release status --json --verbose"}'
  const split = 16
  const cursor = {
    type: 'more_body' as const,
    body: {
      address: pending.address,
      field: 'tool_arguments' as const,
      member_index: 0,
      offset_bytes: String(split),
    },
  }
  const proposal = {
    ...pending,
    projected_body_bytes: 128 + split,
    body: {
      ...pending.body,
      tools: pending.body.tools.map((tool) => ({
        ...tool,
        arguments: {
          text: text.slice(0, split),
          offset_bytes: '0',
          total_bytes: String(text.length),
          continuation: cursor.body,
        },
      })),
    },
  }
  const entries = [attempts[0], proposal].filter((item) => item !== undefined)
  await turnApi(page, undefined, entries)
  let release = () => {}
  const growth = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname === `/api/sessions/${detailSessionId}/follow`) {
      await growth
      return route.fulfill({
        contentType: 'application/x-ndjson',
        body:
          [
            { kind: 'snapshot', snapshot: { ...detailLive, observed_through: '2' } },
            {
              kind: 'durable',
              cursor: '4',
              address: { event_sequence: '4' },
              event_kind: 'tool_batch_transition',
            },
          ]
            .map((event) => JSON.stringify(event))
            .join('\n') + '\n',
      })
    }
    if (url.pathname.endsWith('/timeline')) {
      const window = transcriptFixture(url, entries.length) as WebSessionTimelineWindow
      const items = window.items.map((item) => {
        const kind =
          entries.find((entry) => entry.address.event_sequence === item.address.event_sequence)
            ?.kind ?? item.kind
        return { ...item, kind, projected_structured_bytes: 64 + kind.length }
      })
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
    if (
      url.pathname.endsWith('/timeline-detail') &&
      (url.searchParams.get('cursor_address') ?? url.searchParams.get('first')) === '2'
    ) {
      const continued = url.searchParams.get('cursor_field') === 'tool_arguments'
      return route.fulfill({
        json: detailPage(
          [
            continued
              ? {
                  ...proposal,
                  projected_body_bytes: 128 + text.length - split,
                  body: {
                    ...proposal.body,
                    tools: proposal.body.tools.map((tool) => ({
                      ...tool,
                      arguments: {
                        text: text.slice(split),
                        offset_bytes: String(split),
                        total_bytes: String(text.length),
                        continuation: null,
                      },
                    })),
                  },
                }
              : proposal,
          ],
          continued ? null : cursor,
        ),
      })
    }
    if (
      url.pathname.endsWith('/timeline-detail') &&
      ['3', '4'].includes(url.searchParams.get('first') ?? '')
    ) {
      const items = entries.filter(
        (item) => item.address.event_sequence === url.searchParams.get('first'),
      )
      const item = items[0]
      return route.fulfill({
        json: detailPage(items, {
          ...resultCursor,
          body: {
            ...resultCursor.body,
            address: item?.address ?? { event_sequence: '3' },
            field: url.searchParams.get('first') === '3' ? 'tool_failure' : 'tool_result',
          },
        }),
      })
    }
    if (url.pathname === `/api/sessions/${detailSessionId}` || url.pathname.endsWith('/live'))
      return route.fulfill({ json: transcriptFixture(url, entries.length) })
    return route.fallback()
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const chips = transcript.getByRole('button', {
    name: /^Open turn details for exec_command(?: · .+)?$/,
    exact: true,
  })
  await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
  const continued = transcript.getByRole('region', { name: 'More message text', exact: true })
  await expect(continued).toContainText(text.slice(split))
  const retained = await continued.elementHandle()
  if (!retained) throw new Error('Continued text missing')
  for (const [index, item] of [first, retry].entries()) {
    if (item.body.type !== 'tool_batch') throw new Error('Physical attempt missing')
    entries.push({
      ...item,
      projected_body_bytes: 128 + text.length,
      address: { event_sequence: String(index + 3) },
      body: {
        ...item.body,
        tools: item.body.tools.map((tool) => ({
          ...tool,
          arguments: detailExcerpt(text),
          evidence:
            tool.evidence.type === 'physical_attempt'
              ? { ...tool.evidence, result: null, failure: null }
              : tool.evidence,
        })),
      },
    })
  }
  release()
  await expect(chips).toHaveCount(2)
  await expect(continued).toContainText(text.slice(split))
  expect(await retained.evaluate((element) => element.isConnected)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('request-physical-disclosure.png') })
  expect(problems).toEqual([])
})

for (const [level, command] of [
  ['Summary', 'Show transcript results'],
  ['Tools', 'Show condensed transcript detail'],
  ['All details', 'Show full transcript detail'],
] as const) {
  test(`clears local turn overrides when reapplying ${level} from the palette`, async ({
    page,
  }) => {
    await turnApi(page)
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    const selected = page.getByRole('radio', { name: level, exact: true })
    await selected.check()
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    const expand = transcript.getByRole('button', { name: 'Open turn details', exact: true })
    const collapse = transcript.getByRole('button', { name: 'Collapse turn', exact: true })
    if (level === 'All details') {
      await collapse.click()
      await expect(expand).toBeVisible()
    } else {
      await expand.click()
      await expect(collapse).toBeVisible()
    }
    // An unrelated global command must preserve the local override.
    await page.getByRole('button', { name: 'Open command palette', exact: true }).click()
    await page
      .getByRole('dialog', { name: 'Command palette' })
      .getByRole('button', { name: /Use light theme/ })
      .click()
    await expect(level === 'All details' ? expand : collapse).toBeVisible()
    await page.getByRole('button', { name: 'Open command palette', exact: true }).click()
    await page
      .getByRole('dialog', { name: 'Command palette' })
      .getByRole('button', { name: new RegExp(command) })
      .click()
    await expect(selected).toBeChecked()
    await expect(level === 'All details' ? collapse : expand).toBeVisible()
    await expect(level === 'All details' ? expand : collapse).toHaveCount(0)
    if (level === 'Tools')
      await expect(
        transcript.getByRole('region', { name: 'exec_command details', exact: true }),
      ).toContainText('passed')
  })
}
test('does not offer a goal-text continuation inside a tool disclosure', async ({ page }) => {
  const item = detailItems[1]
  if (item?.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
  const tool = {
    ...item,
    body: {
      ...item.body,
      tools: item.body.tools.map((entry) => ({
        ...entry,
        evidence: { type: 'request_only' as const },
      })),
    },
  }
  await turnApi(
    page,
    undefined,
    [detailItems[0], tool].filter((entry) => entry !== undefined),
  )
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('first') !== '2') return route.fallback()
    return route.fulfill({
      json: detailPage([tool], {
        ...resultCursor,
        body: { ...resultCursor.body, field: 'goal_text' },
      }),
    })
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'release status',
  )
  await expect(transcript.getByRole('button', { name: 'Read more', exact: true })).toHaveCount(0)
  await expect(
    transcript.getByRole('button', { name: 'Show more tools', exact: true }),
  ).toHaveCount(0)
})

test('stops nested tool output reading before the next goal', async ({ page }, testInfo) => {
  const problems: string[] = []
  page.on('pageerror', (error) => problems.push(error.message))
  page.on('console', (message) => {
    if (message.type() === 'error') problems.push(message.text())
  })
  const reads: string[] = []
  const item = toolResultItem()
  if (item.body.type !== 'tool_batch') throw new Error('Tool fixture missing')
  const body = item.body
  const tool = body.tools[0]
  if (tool?.evidence.type !== 'physical_attempt') throw new Error('Physical tool missing')
  const evidence = tool.evidence
  const continued = {
    ...resultCursor,
    body: { ...resultCursor.body, offset_bytes: '5' },
  }
  const goal = {
    ...resultCursor,
    body: { ...resultCursor.body, field: 'goal_text' as const },
  }
  await turnApi(page)
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    const field = url.searchParams.get('cursor_field')
    if (!field) return route.fallback()
    reads.push(field)
    if (field !== 'tool_result') return route.abort()
    const final = url.searchParams.get('cursor_offset') === '5'
    const text = final ? 'output' : 'tool '
    return route.fulfill({
      json: detailPage(
        [
          {
            ...item,
            projected_body_bytes: 128 + text.length,
            body: {
              ...body,
              tools: [
                {
                  ...tool,
                  evidence: {
                    ...evidence,
                    result: {
                      text,
                      offset_bytes: final ? '5' : '0',
                      total_bytes: '11',
                      continuation: final ? null : continued.body,
                    },
                  },
                },
              ],
            },
          },
        ],
        final ? goal : continued,
      ),
    })
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const chips = transcript.getByRole('region', { name: 'Tools used' })
  await expect(
    chips.getByRole('region', { name: 'exec_command details', exact: true }),
  ).toContainText('tool')
  await chips.getByRole('button', { name: 'Read more', exact: true }).click()
  const reader = chips.getByRole('region', { name: 'More message text', exact: true })
  await expect(reader.getByText('tool', { exact: true })).toBeVisible()
  await chips.getByRole('button', { name: 'Continue reading', exact: true }).click()
  await expect(chips.getByText('output', { exact: true })).toBeVisible()
  await expect(chips.getByRole('button', { name: 'Continue reading', exact: true })).toHaveCount(0)
  await expect(chips.getByRole('button', { name: 'Show more tools', exact: true })).toHaveCount(0)
  expect(reads).toEqual(['tool_result', 'tool_result', 'tool_result'])
  await page.screenshot({ path: testInfo.outputPath('tool-stops-before-goal.png') })
  expect(problems).toEqual([])
})

test('identifies payload-free physical attempt states after a successful retry', async ({
  page,
}, testInfo) => {
  const problems: string[] = []
  page.on('pageerror', (error) => problems.push(error.message))
  page.on('console', (message) => {
    if (message.type() === 'error') problems.push(message.text())
  })
  const items = retriedToolItems().map((item) => {
    if (item.body.type !== 'tool_batch') return item
    const tool = item.body.tools[0]
    if (tool?.evidence.type !== 'physical_attempt') return item
    const evidence =
      tool.evidence.state === 'known_failed'
        ? {
            ...tool.evidence,
            cause: 'crash_lost' as const,
            failure_present: false,
            failure: null,
          }
        : {
            ...tool.evidence,
            result_present: false,
            result: null,
          }
    return {
      ...item,
      projected_body_bytes: 128 + Number(tool.arguments?.total_bytes ?? '0'),
      body: {
        ...item.body,
        tools: [
          {
            ...tool,
            evidence,
          },
        ],
      },
    }
  })
  await turnApi(page, undefined, items)
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    const item = items.find((item) => item.address.event_sequence === url.searchParams.get('first'))
    if (!item) return route.fallback()
    const body = item.body
    const tool = body.type === 'tool_batch' ? body.tools[0] : undefined
    if (
      body.type !== 'tool_batch' ||
      tool?.evidence.type !== 'physical_attempt' ||
      !tool.evidence.result_present
    )
      return route.fulfill({ json: detailPage([item]) })
    const continued = url.searchParams.has('cursor_field')
    const excerpt = continued ? tool.evidence.result : tool.arguments
    return route.fulfill({
      json: detailPage(
        [
          {
            ...item,
            projected_body_bytes: 128 + Number(excerpt?.total_bytes ?? '0'),
            body: {
              ...body,
              tools: [
                {
                  ...tool,
                  arguments: continued ? null : tool.arguments,
                  evidence: { ...tool.evidence, result: continued ? tool.evidence.result : null },
                },
              ],
            },
          },
        ],
        continued
          ? null
          : { ...resultCursor, body: { ...resultCursor.body, address: item.address } },
      ),
    })
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const chips = transcript.getByRole('button', {
    name: /^Open turn details for exec_command(?: · .+)?$/,
    exact: true,
  })
  await expect(chips).toHaveCount(2)
  await expect(chips.first()).toHaveText('exec_command · Failed')
  await expect(chips.first()).toHaveAccessibleName('Open turn details for exec_command · Failed')
  await expect(chips.last()).toHaveText('exec_command · Completed')
  await expect(chips.last()).toHaveAccessibleName('Open turn details for exec_command · Completed')
  const slots = transcript.locator('.session-tool-slot')
  await expect(slots.first().locator('.session-turn-outcome')).toHaveText(
    'Failure · Attempt lost on restart',
  )
  await expect(slots.first().getByRole('button', { name: 'Read more', exact: true })).toHaveCount(0)
  await expect(slots.last().locator('.session-turn-outcome')).toHaveText('Completed')
  await expect(slots.last().getByRole('button', { name: 'Read more', exact: true })).toHaveCount(0)
  await slots.first().locator('.session-turn-outcome').scrollIntoViewIfNeeded()
  await page.screenshot({ path: testInfo.outputPath('textless-tool-failure.png') })
  expect(problems).toEqual([])
})

for (const level of ['Summary', 'Tools']) {
  for (const control of ['Continue reading', 'Close details']) {
    test(`Escape closes the ${level} continuation from ${control} and restores its opener`, async ({
      page,
    }) => {
      const state = await continuedMessageApi(page)
      await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
      await page.getByRole('radio', { name: level, exact: true }).check()
      const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
      const opener = transcript.getByRole('button', { name: 'Read more', exact: true })
      await opener.click()
      const reader = transcript.getByRole('region', { name: 'More message text', exact: true })
      await expect(reader).toContainText('b')
      await reader.getByRole('button', { name: control, exact: true }).press('Escape')
      await expect(reader).toHaveCount(0)
      await expect(opener).toBeFocused()
      await expect(transcript).toBeVisible()
      await expect(page).toHaveURL(/workspace=true/)
      state.failContinuation = true
      await opener.click()
      await expect(transcript.getByRole('alert')).toContainText('Details could not be loaded.')
      await expect(transcript.locator('.session-message-text')).toHaveText(['a'])
    })
  }
}

test('Escape closes the focused tool reader and restores Read more', async ({ page }) => {
  await toolGoalApi(page, 'blocked')
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'Tools', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const opener = transcript.getByRole('button', { name: 'Read more', exact: true })
  await opener.click()
  const reader = transcript.getByRole('region', { name: 'More message text', exact: true })
  await expect(reader).toContainText('passed')
  await reader.getByRole('button', { name: 'Close details', exact: true }).press('Escape')
  await expect(reader).toHaveCount(0)
  await expect(opener).toBeFocused()
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toBeVisible()
  await expect(page).toHaveURL(/workspace=true/)
})
for (const [state, label] of [
  ['ambiguous', 'Outcome unknown'],
  ['awaiting_child', 'Waiting for child session'],
] as const) {
  test(`shows the ${state} physical attempt state without payload text`, async ({
    page,
  }, testInfo) => {
    const item = toolResultItem()
    const input = detailItems[0]
    if (item.body.type !== 'tool_batch' || !input) throw new Error('Tool fixture missing')
    const tool = item.body.tools[0]
    if (tool?.evidence.type !== 'physical_attempt') throw new Error('Physical attempt missing')
    const args = detailExcerpt('{"cmd":"check status"}')
    const physical: WebSessionTimelineDetail = {
      ...item,
      projected_body_bytes: 128 + Number(args.total_bytes),
      body: {
        ...item.body,
        tools: [
          {
            ...tool,
            arguments: args,
            evidence: {
              ...tool.evidence,
              state,
              effect_posture: 'external_effect',
              cause: null,
              result: null,
              result_present: false,
              failure: null,
              failure_present: false,
            },
          },
        ],
      },
    }
    await turnApi(page, undefined, [input, physical])
    await page.route('**/timeline-detail?**', (route) => {
      const url = new URL(route.request().url())
      return url.searchParams.get('first') === physical.address.event_sequence
        ? route.fulfill({ json: detailPage([physical]) })
        : route.fallback()
    })
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    await page.getByRole('radio', { name: 'Tools', exact: true }).check()
    const details = transcript.getByRole('region', { name: 'exec_command details', exact: true })
    await expect(details.getByRole('region', { name: 'Arguments', exact: true })).toContainText(
      'check status',
    )
    await expect(details.getByText(label, { exact: true })).toBeVisible()
    await expect(details.getByRole('region', { name: 'Failure', exact: true })).toHaveCount(0)
    await expect(transcript.getByRole('button', { name: 'Read more', exact: true })).toHaveCount(0)
    await page.screenshot({ path: testInfo.outputPath(`${state}-tool-state.png`) })
  })
}

for (const close of ['button', 'Escape']) {
  test(`restores an unmounted turn heading after collapsing a distant segment by ${close}`, async ({
    page,
  }) => {
    const source = detailItems[0]
    const tool = detailItems[1]
    if (source?.body.type !== 'user_input' || !tool) throw new Error('Turn fixture missing')
    const originalTurnId = source.body.turn_id
    const entries = Array.from({ length: 24 }, (_, index) => {
      const address = { event_sequence: String(index + 1) }
      if (index === 23) return { ...tool, address }
      const text = detailExcerpt(`Message ${index + 1}`)
      return {
        ...source,
        address,
        projected_body_bytes: 128 + Number(text.total_bytes),
        body: {
          ...source.body,
          turn_id:
            index === 0
              ? originalTurnId
              : `00000000-0000-0000-0000-${String(index).padStart(12, '0')}`,
          text,
          attachments: [],
        },
      }
    })
    await turnApi(page, undefined, entries)
    await page.route('**/timeline?**', (route) => {
      const window = transcriptFixture(
        new URL(route.request().url()),
        24,
      ) as WebSessionTimelineWindow
      const items = window.items.map((item) => {
        const kind = item.address.event_sequence === '24' ? tool.kind : item.kind
        return { ...item, kind, projected_structured_bytes: 64 + kind.length }
      })
      return route.fulfill({
        json: {
          ...window,
          session_id: detailSessionId,
          items,
          projected_structured_bytes: items.reduce(
            (sum, item) => sum + item.projected_structured_bytes,
            0,
          ),
        },
      })
    })
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    for (const expected of ['Message 9', 'Message 1']) {
      await expect
        .poll(async () => {
          await transcript.evaluate((element) => {
            element.scrollTop = 0
            element.dispatchEvent(new Event('scroll'))
          })
          return transcript.getByText(expected, { exact: true }).isVisible()
        })
        .toBe(true)
    }
    const first = transcript
      .locator('[data-transcript-turn]')
      .filter({ has: page.getByText('Message 1', { exact: true }) })
    await first.getByRole('button', { name: 'Open turn details', exact: true }).click()
    await transcript.evaluate((element) => {
      element.scrollTop = element.scrollHeight
    })
    const last = transcript
      .locator('[data-transcript-turn]')
      .filter({ has: page.locator('[data-event-sequence="24"]') })
    const collapse = last.getByRole('button', { name: 'Collapse turn', exact: true })
    await expect(collapse).toBeVisible()
    await collapse.focus()
    await expect(first).toHaveCount(0)
    if (close === 'Escape') await collapse.press('Escape')
    else await collapse.click()
    await expect(
      first.getByRole('button', { name: 'Open turn details', exact: true }),
    ).toBeFocused()
    await expect(first).toBeInViewport()
    await expect(last).toHaveCount(0)
    await expect(page.getByRole('radio', { name: 'Summary', exact: true })).toBeChecked()
  })
}

test('All details renders typed facts and requires explicit raw disclosures', async ({ page }) => {
  const input = detailItems[0]
  const approval = detailItems[2]
  if (input?.body.type !== 'user_input' || !approval) throw new Error('Turn fixture missing')
  const overlay = {
    reasoning_level: { kind: 'inherit' },
    fast_mode: { kind: 'inherit' },
    service_tier: { kind: 'inherit' },
  } as const
  const settings: WebSessionTimelineDetail = {
    address: { event_sequence: '4' },
    kind: 'turn_model_settings_resolved',
    projected_body_bytes: 128,
    body: {
      type: 'model_settings',
      detail: {
        type: 'turn_resolved',
        accepted_input_id: detailSessionId,
        turn_id: input.body.turn_id,
        defaults_version: '1',
        requested_model: { kind: 'direct', selection_id: detailSessionId },
        selected_direct_id: detailSessionId,
        per_call_override: overlay,
        settings: {
          precedence: {
            per_call: overlay,
            session: overlay,
            profile: overlay,
            global_default: overlay,
          },
          effective: { reasoning_level: null, fast_mode: 'disabled', service_tier: null },
        },
        adjustments: [],
      },
    },
  }
  await turnApi(page, undefined, [input, approval, settings])
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const decision = transcript.locator('[data-event-sequence="3"]')
  await expect(decision.getByText('Decision', { exact: true })).toBeVisible()
  await expect(decision.getByText('Deny', { exact: true })).toBeVisible()
  const model = transcript.locator('[data-event-sequence="4"]')
  await expect(model.getByText('Reasoning level', { exact: true })).toBeVisible()
  await expect(model.getByText('Disabled', { exact: true })).toBeVisible()
  await expect(transcript.locator('.session-event-facts:visible, code:visible')).toHaveCount(0)
  await decision.getByText('Raw event data', { exact: true }).click()
  await expect(decision.locator('.session-event-facts')).toBeVisible()
  await expect(decision.locator('.session-event-facts')).toContainText(
    '"approval_judge_escalated": true',
  )
  await decision.getByText('Raw event data', { exact: true }).click()
  await expect(decision.locator('.session-event-facts')).toBeHidden()
  const rawSetting = model.getByText('Raw setting data', { exact: true }).first()
  await rawSetting.click()
  await expect(model.locator('code:visible')).toHaveCount(1)
  await rawSetting.click()
  await expect(transcript.locator('.session-event-facts:visible, code:visible')).toHaveCount(0)
})

test('All details shows model and failed tool facts without payload text', async ({ page }) => {
  const input = detailItems[0]
  const model = detailItems[3]
  const original = retriedToolItems().find(
    (item) =>
      item.body.type === 'tool_batch' &&
      item.body.tools.some(
        (tool) =>
          tool.evidence.type === 'physical_attempt' && tool.evidence.state === 'known_failed',
      ),
  )
  if (!input || model?.body.type !== 'model_call' || original?.body.type !== 'tool_batch')
    throw new Error('Model and failed-tool fixture missing')
  const tools = original.body.tools.map((tool) => ({
    ...tool,
    arguments: detailExcerpt(''),
    evidence:
      tool.evidence.type === 'physical_attempt'
        ? { ...tool.evidence, failure: null, failure_present: false }
        : tool.evidence,
  }))
  const failed: WebSessionTimelineDetail = {
    ...original,
    address: { event_sequence: '2' },
    projected_body_bytes: 128,
    body: { ...original.body, tools },
  }
  const textless: WebSessionTimelineDetail = {
    ...model,
    projected_body_bytes: 128,
    body: {
      ...model.body,
      response: null,
      usage: {
        input_tokens: '12',
        output_tokens: '3',
        cache_creation_input_tokens: '4',
        cache_read_input_tokens: '5',
      },
    },
  }
  const entries = [input, failed, textless]
  await turnApi(page, undefined, entries)
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    const sequence = url.searchParams.get('cursor_address') ?? url.searchParams.get('first')
    const item = entries.find((entry) => entry.address.event_sequence === sequence)
    return item && item !== input ? route.fulfill({ json: detailPage([item]) }) : route.fallback()
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const call = transcript.locator('[data-event-sequence="4"]')
  const fact = (label: string) =>
    call
      .locator('dl > div')
      .filter({ has: page.getByText(label, { exact: true }) })
      .locator('dd')
  await expect(fact('Model')).toHaveText(model.body.model_identity_id)
  await expect(fact('State')).toHaveText('Finished · Completed')
  await expect(fact('Input tokens')).toHaveText('12')
  await expect(fact('Output tokens')).toHaveText('3')
  await expect(fact('Cache creation input tokens')).toHaveText('4')
  await expect(fact('Cache read input tokens')).toHaveText('5')
  const attempt = transcript
    .locator('[data-event-sequence="2"]')
    .getByRole('region', { name: 'Tool requests', exact: true })
  await expect(attempt.getByText('Failed', { exact: true })).toBeVisible()
  await expect(attempt.getByText('Attempt lost on restart', { exact: true })).toBeVisible()
  await expect(attempt.getByText('Approval', { exact: true })).toBeVisible()
  await expect(attempt.getByText('Effect', { exact: true })).toBeVisible()
  await expect(transcript.locator('.session-event-facts:visible')).toHaveCount(0)
})

test('All details retains every response chunk with one set of model facts', async ({ page }) => {
  const input = detailItems[0]
  const model = detailItems[3]
  if (!input || model?.body.type !== 'model_call') throw new Error('Model fixture missing')
  const body = model.body
  const chunk = (offset: number) => {
    const continuation =
      offset < 2
        ? {
            address: model.address,
            field: 'model_response' as const,
            member_index: 0,
            offset_bytes: String(offset + 1),
          }
        : null
    return {
      ...model,
      projected_body_bytes: 129,
      body: {
        ...body,
        response: {
          text: 'abc'[offset] ?? '',
          offset_bytes: String(offset),
          total_bytes: '3',
          continuation,
        },
      },
    }
  }
  await turnApi(page, undefined, [input, chunk(0)])
  await page.route('**/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if ((url.searchParams.get('cursor_address') ?? url.searchParams.get('first')) !== '4')
      return route.fallback()
    const item = chunk(Number(url.searchParams.get('cursor_offset') ?? '0'))
    const continuation = item.body.response.continuation
    return route.fulfill({
      json: detailPage([item], continuation ? { type: 'more_body', body: continuation } : null),
    })
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  const call = page
    .getByRole('region', { name: 'Session transcript', exact: true })
    .locator('[data-event-sequence="4"]')
  await expect(call.locator('.session-message-text')).toHaveText(['a'])
  await call.getByRole('button', { name: 'Continue reading', exact: true }).click()
  await call.getByRole('button', { name: 'Continue reading', exact: true }).click()
  await expect(call.locator('.session-message-text')).toHaveText(['a', 'b', 'c'])
  await expect(call.getByText('Model', { exact: true })).toHaveCount(1)
  await expect(call.getByText('Input tokens', { exact: true })).toHaveCount(1)
})

for (const level of ['Summary', 'Tools', 'All details']) {
  test(`Escape collapses the focused ${level} turn while the desktop inspector stays open`, async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1440, height: 900 })
    await turnApi(page)
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    await page.getByRole('radio', { name: level, exact: true }).check()
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    if (level !== 'All details')
      await transcript.getByRole('button', { name: 'Open turn details', exact: true }).click()
    const opener = page.getByRole('button', { name: 'Open artifact inspector', exact: true })
    await opener.click()
    const inspector = page.getByRole('complementary', { name: 'Inspector', exact: true })
    await expect(inspector).toBeVisible()
    await transcript.getByRole('button', { name: 'Collapse turn', exact: true }).press('Escape')
    await expect(
      transcript.getByRole('button', { name: 'Open turn details', exact: true }),
    ).toBeFocused()
    await expect(inspector).toBeVisible()
    await expect(page.getByRole('radio', { name: level, exact: true })).toBeChecked()
    await inspector
      .getByRole('button', { name: 'Close artifact inspector', exact: true })
      .press('Escape')
    await expect(inspector).toHaveCount(0)
    await expect(opener).toBeFocused()
    await expect(transcript).toBeVisible()
  })
}
