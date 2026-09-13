import type { WebSessionTimelineWindow } from '../src/generated/web-contract.mjs'
import { transcriptFixture } from '../src/session-timeline/transcript.fixture'
import { retriedToolItems, toolGoalApi, turnApi } from '../src/session-timeline/turns.fixture'
import { expect, test } from './fontTest'
import {
  detailCallId,
  detailExcerpt,
  detailItems,
  detailPage,
  detailSessionId,
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

test('restores the selected Tools level after collapsing a tool-opened turn', async ({ page }) => {
  await turnApi(page)
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const tools = page.getByRole('radio', { name: 'Tools', exact: true })
  await tools.check()
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const summary = transcript.getByRole('region', { name: 'exec_command details', exact: true })
  const openTool = transcript.getByRole('button', {
    name: 'Open turn details for exec_command',
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
    'exec_command',
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
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const chips = transcript.getByRole('button', { name: 'exec_command', exact: true })
  await expect(chips).toHaveCount(2)
  await chips.first().click()
  await chips.last().click()
  const slots = transcript.locator('.session-tool-slot')
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

for (const level of ['Summary', 'Tools']) {
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
    const chip = transcript.getByRole('button', { name: 'exec_command', exact: true })
    if (level === 'Summary') await chip.click()
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
    if (level === 'Summary') {
      await chip.click()
      await chip.click()
      await expect(reader).toHaveCount(0)
      await transcript.getByRole('button', { name: 'Read more', exact: true }).click()
      await expect(reader.locator('pre')).toHaveText(['second chunk'])
    }
  })
}

for (const level of ['Summary', 'Tools']) {
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
    if (level === 'Summary')
      await transcript.getByRole('button', { name: 'exec_command', exact: true }).click()
    const summary = transcript.getByRole('region', { name: 'exec_command details', exact: true })
    await expect(summary.getByRole('region', { name: 'Output', exact: true })).toContainText(
      'passed',
    )
    expect(addresses).toEqual(['4'])
    await page.screenshot({ path: testInfo.outputPath('terminal-tool-output.png') })
  })
}

for (const changed of ['attempt', 'attachments', 'total']) {
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
                text: changed === 'total' ? text : input.body.text,
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

test('keeps the retained turn row and open tool when earlier history is prepended', async ({
  page,
}) => {
  const input = detailItems[0]
  const tool = detailItems[1]
  if (input?.body.type !== 'user_input' || !tool) throw new Error('Fixture missing')
  const entries = Array.from({ length: 16 }, (_, index) => {
    const address = { event_sequence: String(index + 1) }
    if (index === 8) return { ...tool, address }
    const text = detailExcerpt(`Message ${index + 1}`)
    return {
      ...input,
      address,
      projected_body_bytes: 128 + Number(text.total_bytes),
      body: { ...input.body, text, attachments: [] },
    }
  })
  await turnApi(page, undefined, entries)
  let release = () => {}
  const olderReady = new Promise<void>((resolve) => {
    release = resolve
  })
  await page.route('**/timeline?**', async (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('anchor') === 'before') await olderReady
    const window = transcriptFixture(url, 16) as WebSessionTimelineWindow
    const items = window.items.map((item) => {
      const kind = item.address.event_sequence === '9' ? tool.kind : item.kind
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
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const chip = transcript.getByRole('button', { name: 'exec_command', exact: true })
  await chip.click()
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
  await expect(chip).toHaveAttribute('aria-expanded', 'true')
  await expect(
    transcript.getByRole('region', { name: 'exec_command details', exact: true }),
  ).toContainText('release status')
})
