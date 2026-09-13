import type { WebSessionTimelineWindow } from '../src/generated/web-contract.mjs'
import { transcriptFixture } from '../src/session-timeline/transcript.fixture'
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
