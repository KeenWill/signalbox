import type {
  WebSessionTimelineDetail,
  WebSessionTimelineWindow,
} from '../src/generated/web-contract.mjs'
import { transcriptFixture, transcriptSessionId } from '../src/session-timeline/transcript.fixture'
import { retriedToolItems, turnApi } from '../src/session-timeline/turns.fixture'
import { expect, test } from './fontTest'
import {
  detailCallId,
  detailExcerpt,
  detailItems,
  detailLive,
  detailPage,
  detailSessionId,
  detailWindow,
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

for (const interleaved of [false, true]) {
  test(`shows final turn text and a tool chip while keeping lifecycle noise closed${interleaved ? ' with interleaved turns' : ''}`, async ({
    page,
  }, testInfo) => {
    const input = detailItems[0]
    if (input?.body.type !== 'user_input') throw new Error('Input fixture missing')
    const text = detailExcerpt('Start the next check after this turn.')
    const entries = interleaved
      ? [
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
      : detailItems
    const window = {
      ...detailWindow,
      items: entries.map(({ address, kind }) => ({
        address,
        kind,
        projected_structured_bytes: 64 + kind.length,
      })),
      projected_structured_bytes: entries.reduce((sum, item) => sum + 64 + item.kind.length, 0),
    }
    const latest = entries.at(-1)?.address.event_sequence ?? '1'
    await page.route('**/api/**', (route) => {
      const url = new URL(route.request().url())
      if (url.pathname.endsWith('/follow'))
        return route.fulfill({ contentType: 'application/x-ndjson', body: '' })
      if (url.pathname === '/api/attention')
        return route.fulfill({
          json: { cursor: '0', summaries: [], continuation_after_session_id: null },
        })
      if (url.pathname.endsWith('/timeline')) return route.fulfill({ json: window })
      if (url.pathname.endsWith('/live'))
        return route.fulfill({ json: { ...detailLive, observed_through: latest } })
      if (url.pathname.endsWith('/timeline-detail')) {
        const item = entries.find(
          (entry) => entry.address.event_sequence === url.searchParams.get('first'),
        )
        const items = item
          ? [
              item.body.type === 'user_input'
                ? { ...item, body: { ...item.body, attachments: [] } }
                : item,
            ]
          : []
        return route.fulfill({
          json: detailPage(items, item?.body.type === 'tool_batch' ? resultCursor : null),
        })
      }
      if (url.pathname === `/api/sessions/${detailSessionId}`)
        return route.fulfill({
          json: {
            session_id: detailSessionId,
            supervision: null,
            repository_watch: null,
            workspace_root_kind: null,
            sizes: {
              item_count: String(entries.length),
              projected_text_bytes: '300',
              projected_structured_bytes: String(window.projected_structured_bytes),
              referenced_blob_count: '0',
              referenced_blob_bytes: '0',
            },
            first_address: { event_sequence: '1' },
            latest_address: { event_sequence: latest },
            observed_through: latest,
            work: { active_turn_count: '0', queued_turn_count: '0' },
          },
        })
      return route.fulfill({ json: transcriptFixture(url) })
    })
    await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    await expect(
      transcript.getByText('The release checks passed. Publishing remains unapproved.'),
    ).toBeVisible()
    await expect(
      transcript.getByText('Inspect the release status and retain the result.'),
    ).toHaveCount(1)
    await expect(transcript.getByText('Turn completed', { exact: true })).toHaveCount(0)
    const chip = transcript.getByRole('button', { name: /^exec_command(?: · .+)?$/ })
    await expect(chip).toHaveAttribute('aria-expanded', 'false')
    await chip.click()
    await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
      'release status',
    )
    await expect(chip).toHaveAttribute('aria-expanded', 'true')
    await expect(transcript.locator('.session-message-text, .session-tool-slot')).toHaveText([
      'Inspect the release status and retain the result.',
      /release status/,
      ...(interleaved ? ['Start the next check after this turn.'] : []),
      'The release checks passed. Publishing remains unapproved.',
    ])
    await page.screenshot({ path: testInfo.outputPath('turn-summary.png') })
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
    'exec_command · Completed',
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
  const chips = transcript.getByRole('button', { name: /^exec_command(?: · .+)?$/ })
  await expect(chips).toHaveCount(2)
  await expect(chips.first()).toHaveText('exec_command · Failed')
  await expect(chips.first()).toHaveAccessibleName('exec_command · Failed')
  await expect(chips.last()).toHaveText('exec_command · Completed')
  await expect(chips.last()).toHaveAccessibleName('exec_command · Completed')
  await chips.first().click()
  await chips.last().click()
  const slots = transcript.locator('.session-tool-slot')
  await expect(slots.first().getByText('Failure · Attempt lost on restart')).toBeVisible()
  await slots
    .first()
    .getByRole('button', { name: /^Read more(?: .+)?$/ })
    .click()
  await slots
    .last()
    .getByRole('button', { name: /^Read more(?: .+)?$/ })
    .click()
  await expect(slots.first().getByRole('region', { name: 'Failure', exact: true })).toContainText(
    'Runner disconnected during the release check.',
  )
  await expect(slots.last().getByRole('region', { name: 'Output', exact: true })).toContainText(
    'passed',
  )
  await expect(transcript.locator('.session-message-text, .session-tool-slot')).toHaveText([
    'Inspect the release status and retain the result.',
    /Runner disconnected/,
    'Check the next release too.',
    /checks.*passed/,
    'The release checks passed. Publishing remains unapproved.',
  ])
  await page.screenshot({ path: testInfo.outputPath('retried-tool.png') })
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
  const chips = transcript.getByRole('button', { name: /^exec_command(?: · .+)?$/ })
  await chips.click()
  await transcript.getByRole('button', { name: /^Read more(?: .+)?$/ }).click()
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
  await expect(chips.first()).toHaveAttribute('aria-expanded', 'true')
  await expect(chips.last()).toHaveAttribute('aria-expanded', 'false')
  await expect(continued).toContainText(text.slice(split))
  expect(await retained.evaluate((element) => element.isConnected)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('request-physical-disclosure.png') })
  await chips.last().click()
  await expect(chips.first()).toHaveAttribute('aria-expanded', 'false')
  await expect(chips.last()).toHaveAttribute('aria-expanded', 'true')
  expect(problems).toEqual([])
})

test('distinguishes proposal arguments from the continuation that reaches tool output', async ({
  page,
}, testInfo) => {
  const problems: string[] = []
  page.on('pageerror', (error) => problems.push(error.message))
  page.on('console', (message) => {
    if (message.type() === 'error') problems.push(message.text())
  })
  const attempts = retriedToolItems()
  const pending = attempts[1]
  const first = attempts[4]
  if (pending?.body.type !== 'tool_batch' || !first) throw new Error('Attempt fixture missing')
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
              cursor: '3',
              address: { event_sequence: '3' },
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
    if (url.pathname.endsWith('/timeline-detail') && url.searchParams.get('first') === '3') {
      const item = entries.find((entry) => entry.address.event_sequence === '3')
      if (item?.body.type !== 'tool_batch') throw new Error('Result fixture missing')
      const field = url.searchParams.get('cursor_field')
      if (field === 'tool_result') {
        if (first.body.type !== 'tool_batch') throw new Error('Result fixture missing')
        return route.fulfill({
          json: detailPage([
            {
              ...first,
              address: item.address,
              projected_body_bytes: toolResultItem().projected_body_bytes,
              body: {
                ...first.body,
                tools: first.body.tools.map((tool) => ({ ...tool, arguments: null })),
              },
            },
          ]),
        })
      }
      const continued = field === 'tool_arguments'
      return route.fulfill({
        json: detailPage(
          [
            {
              ...item,
              projected_body_bytes: 128 + (continued ? text.length - split : split),
              body: {
                ...item.body,
                tools: item.body.tools.map((tool) => ({
                  ...tool,
                  arguments: continued
                    ? {
                        text: text.slice(split),
                        offset_bytes: String(split),
                        total_bytes: String(text.length),
                        continuation: null,
                      }
                    : tool.arguments,
                })),
              },
            },
          ],
          continued
            ? { ...resultCursor, body: { ...resultCursor.body, address: item.address } }
            : { ...cursor, body: { ...cursor.body, address: item.address } },
        ),
      })
    }
    if (url.pathname === `/api/sessions/${detailSessionId}` || url.pathname.endsWith('/live'))
      return route.fulfill({ json: transcriptFixture(url, entries.length) })
    return route.fallback()
  })
  await page.goto(`/sessions?workspace=true&session=${detailSessionId}`)
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const chips = transcript.getByRole('button', { name: /^exec_command(?: · .+)?$/ })
  await chips.click()
  await expect(
    transcript.getByRole('button', { name: 'Read more arguments', exact: true }),
  ).toBeVisible()
  if (first.body.type !== 'tool_batch') throw new Error('Physical attempt missing')
  const address = { event_sequence: '3' }
  entries.push({
    ...first,
    address,
    projected_body_bytes: 128 + split,
    body: {
      ...first.body,
      tools: first.body.tools.map((tool) => ({
        ...tool,
        arguments: {
          text: text.slice(0, split),
          offset_bytes: '0',
          total_bytes: String(text.length),
          continuation: { ...cursor.body, address },
        },
        evidence:
          tool.evidence.type === 'physical_attempt'
            ? { ...tool.evidence, result: null }
            : tool.evidence,
      })),
    },
  })
  release()
  await expect(chips).toHaveText('exec_command · Completed')
  const argumentsReader = transcript.getByRole('button', {
    name: 'Read more arguments',
    exact: true,
  })
  const outputReader = transcript.getByRole('button', {
    name: 'Read more arguments and output',
    exact: true,
  })
  await expect(argumentsReader).toBeVisible()
  await expect(outputReader).toBeVisible()
  await expect(transcript.getByRole('button', { name: /^Read more/ })).toHaveCount(2)
  await argumentsReader.click()
  const continued = transcript.getByRole('region', { name: 'More message text', exact: true })
  await expect(continued).toContainText(text.slice(split))
  await expect(
    continued.getByRole('button', { name: 'Continue reading', exact: true }),
  ).toHaveCount(0)
  await continued.getByRole('button', { name: 'Close details', exact: true }).click()
  await outputReader.click()
  await expect(continued).toContainText(text.slice(split))
  await continued.getByRole('button', { name: 'Continue reading', exact: true }).click()
  await expect(continued.getByRole('region', { name: 'Output', exact: true })).toContainText(
    'passed',
  )
  await page.screenshot({ path: testInfo.outputPath('distinct-tool-continuations.png') })
  expect(problems).toEqual([])
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
  const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
  const chip = transcript.getByRole('button', { name: /^exec_command(?: · .+)?$/ })
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
  await page.screenshot({ path: testInfo.outputPath('retained-tool-segment.png') })
})

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
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    const chips = transcript.getByRole('region', { name: 'Tools used' })
    await expect(chips.getByRole('button', { name: /^exec_command(?: · .+)?$/ })).toBeVisible()
    expect(reads).toEqual([])
    if (afterOutput) {
      await chips.getByRole('button', { name: /^exec_command(?: · .+)?$/ }).click()
      await chips.getByRole('button', { name: /^Read more(?: .+)?$/ }).click()
      await expect(chips.getByText('First tool output', { exact: true })).toBeVisible()
    }
    await chips.getByRole('button', { name: 'Show more tools', exact: true }).click()
    await expect(chips.getByRole('alert')).toHaveText('More tools could not be loaded.')
    unavailable = false
    await chips.getByRole('button', { name: 'Retry more tools', exact: true }).click()
    const second = chips.getByRole('button', { name: /^read_file(?: · .+)?$/ })
    await expect(second).toBeVisible()
    await second.click()
    await expect(
      chips.getByRole('region', { name: 'read_file details', exact: true }),
    ).toContainText('Arguments for tool 1')
    await expect(chips.getByRole('button', { name: /^exec_command(?: · .+)?$/ })).toHaveAttribute(
      'aria-expanded',
      'false',
    )
    await chips.getByRole('button', { name: 'Show more tools', exact: true }).click()
    const third = chips.getByRole('button', { name: /^apply_patch(?: · .+)?$/ })
    await expect(third).toBeVisible()
    await third.click()
    await expect(
      chips.getByRole('region', { name: 'apply_patch details', exact: true }),
    ).toContainText('Arguments for tool 2')
    await expect(second).toHaveAttribute('aria-expanded', 'false')
    await expect(chips.getByRole('button', { name: 'Show more tools', exact: true })).toHaveCount(0)
    expect(reads).toEqual(afterOutput ? [0, 1, 1, 2] : [1, 1, 2])
    await page.screenshot({ path: testInfo.outputPath('separate-batch-chips.png') })
  })
}

for (const continuedGoal of [false, true]) {
  test(`automatically scans past goal-only batch windows${continuedGoal ? ' with goal continuations' : ''} to earlier conversation`, async ({
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
        const address = { event_sequence: url.searchParams.get('first') ?? '' }
        const continuation = continuedGoal
          ? {
              type: 'more_body' as const,
              body: { address, field: 'goal_text' as const, member_index: 0, offset_bytes: '8' },
            }
          : null
        const text = continuation
          ? {
              ...goal,
              text: goal.text.slice(0, 8),
              continuation: continuation.body,
            }
          : goal
        const item = {
          ...batch,
          address,
          projected_body_bytes: 128 + text.text.length,
          body: {
            ...batch.body,
            tools: [],
            goal_events: [{ type: 'achieved' as const, generation: '1', text }],
          },
        }
        const detail = detailPage([item], continuation)
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
  await transcript.getByRole('button', { name: /^exec_command(?: · .+)?$/ }).click()
  await expect(transcript.getByRole('region', { name: 'exec_command details' })).toContainText(
    'release status',
  )
  await expect(transcript.getByRole('button', { name: /^Read more(?: .+)?$/ })).toHaveCount(0)
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
  const chips = transcript.getByRole('region', { name: 'Tools used' })
  await chips.getByRole('button', { name: /^exec_command(?: · .+)?$/ }).click()
  await chips.getByRole('button', { name: /^Read more(?: .+)?$/ }).click()
  await expect(chips.getByText('tool', { exact: true })).toBeVisible()
  await chips.getByRole('button', { name: 'Continue reading', exact: true }).click()
  await expect(chips.getByText('output', { exact: true })).toBeVisible()
  await expect(chips.getByRole('button', { name: 'Continue reading', exact: true })).toHaveCount(0)
  await expect(chips.getByRole('button', { name: 'Show more tools', exact: true })).toHaveCount(0)
  expect(reads).toEqual(['tool_result', 'tool_result'])
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
  const chips = transcript.getByRole('button', { name: /^exec_command(?: · .+)?$/ })
  await expect(chips).toHaveCount(2)
  await expect(chips.first()).toHaveText('exec_command · Failed')
  await expect(chips.first()).toHaveAccessibleName('exec_command · Failed')
  await expect(chips.last()).toHaveText('exec_command · Completed')
  await expect(chips.last()).toHaveAccessibleName('exec_command · Completed')
  await chips.first().click()
  await chips.last().click()
  const slots = transcript.locator('.session-tool-slot')
  await expect(slots.first().locator('.session-turn-outcome')).toHaveText(
    'Failure · Attempt lost on restart',
  )
  await expect(slots.first().getByRole('button', { name: /^Read more(?: .+)?$/ })).toHaveCount(0)
  await expect(slots.last().locator('.session-turn-outcome')).toHaveText('Completed')
  await expect(slots.last().getByRole('button', { name: /^Read more(?: .+)?$/ })).toHaveCount(0)
  await slots.first().locator('.session-turn-outcome').scrollIntoViewIfNeeded()
  await page.screenshot({ path: testInfo.outputPath('textless-tool-failure.png') })
  expect(problems).toEqual([])
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
    await transcript.getByRole('button', { name: /^exec_command(?: · .+)?$/ }).click()
    const details = transcript.getByRole('region', { name: 'exec_command details', exact: true })
    await expect(details.getByRole('region', { name: 'Arguments', exact: true })).toContainText(
      'check status',
    )
    await expect(details.getByText(label, { exact: true })).toBeVisible()
    await expect(details.getByRole('region', { name: 'Failure', exact: true })).toHaveCount(0)
    await expect(transcript.getByRole('button', { name: /^Read more(?: .+)?$/ })).toHaveCount(0)
    await page.screenshot({ path: testInfo.outputPath(`${state}-tool-state.png`) })
  })
}
