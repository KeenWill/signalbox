import { toolGoalApi, turnApi } from '../src/session-timeline/turns.fixture'
import { expect, test } from './fontTest'
import {
  detailCallId,
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

test('retains continued event text while its virtualized row is unmounted', async ({ page }) => {
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
  await page.route('**/turns/*/timeline-detail?**', (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('cursor_address') !== '1') return route.fallback()
    const offset = url.searchParams.get('cursor_offset')
    const item = entries[0]
    if (!item || item.body.type !== 'user_input') throw new Error('Input fixture missing')
    const continuation =
      offset === '11'
        ? null
        : {
            address: item.address,
            field: 'input_text' as const,
            member_index: 0,
            offset_bytes: '11',
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
                text: offset === '11' ? 'second chunk' : 'first chunk',
                offset_bytes: offset ?? '0',
                total_bytes: '23',
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
  await page.getByRole('radio', { name: 'All details', exact: true }).check()
  const input = transcript.locator('[data-event-sequence="1"]')
  await input.getByRole('button', { name: 'Continue reading' }).click()
  await expect(input.locator('.session-message-text')).toHaveText(['first chunk', 'second chunk'])

  await transcript.evaluate((element) => {
    element.scrollTop = element.scrollHeight
    element.dispatchEvent(new Event('scroll'))
  })
  await expect(input).toHaveCount(0)
  await transcript.evaluate((element) => {
    element.scrollTop = 0
    element.dispatchEvent(new Event('scroll'))
  })
  await expect(input.locator('.session-message-text')).toHaveText(['first chunk', 'second chunk'])
})

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
