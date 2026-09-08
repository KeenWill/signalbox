import type { WebTimelineCreationCause } from '../src/generated/web-contract.mjs'
import { webContractBootstrapFixture } from '../src/product.fixture'
import { expect, type Page, test } from './fontTest'
import {
  detailItems,
  detailLive,
  detailPage,
  detailSessionId,
  detailWindow,
  resultCursor,
  toolResultItem,
} from './session-detail-fixture'

async function openDetails(
  page: Page,
  mismatch = false,
  override = false,
  creation?: WebTimelineCreationCause,
  outcome?: 'reconciliation' | 'retired',
  partialResult = false,
) {
  const items = detailItems.map((item, index) =>
    creation && index === 0
      ? {
          ...item,
          kind: 'session_created' as const,
          projected_body_bytes: 128,
          body: { type: 'session_created' as const, cause: creation, imported_evidence: null },
        }
      : outcome === 'reconciliation' && index === 4
        ? {
            ...item,
            kind: 'turn_reconciliation_required' as const,
            body: {
              type: 'reconciliation' as const,
              turn_id: detailSessionId,
              terminal_frontier_id: detailSessionId,
              operation: { type: 'model_call' as const, model_call_id: detailSessionId },
            },
          }
        : outcome === 'retired' && index === 4
          ? {
              ...item,
              kind: 'goal_turn_retired' as const,
              body: { type: 'event_fact' as const, kind: 'goal_turn_retired' as const },
            }
          : item,
  )
  const windowItems = detailWindow.items.map((item, index) => ({
    ...item,
    kind: items[index]?.kind ?? item.kind,
  }))
  const timelineWindow = {
    ...detailWindow,
    items: windowItems.map((item) => ({
      ...item,
      projected_structured_bytes: 64 + item.kind.length,
    })),
    projected_structured_bytes: windowItems.reduce((sum, item) => sum + 64 + item.kind.length, 0),
  }

  const reads: URL[] = []
  await page.addInitScript(
    ({ sessionId }) => {
      const request = window.fetch
      window.fetch = (input, init) =>
        String(input).endsWith(`/sessions/${sessionId}/follow`)
          ? Promise.resolve(
              new Response(
                new ReadableStream({
                  start(controller) {
                    controller.enqueue(
                      new TextEncoder().encode(
                        `${JSON.stringify({
                          kind: 'snapshot',
                          snapshot: {
                            session_id: sessionId,
                            observed_through: '5',
                            active: null,
                            queued_turn_count: '0',
                            queued_turn_ids: [],
                            reconciliation: null,
                            runner: null,
                          },
                        })}\n`,
                      ),
                    )
                  },
                }),
                { headers: { 'content-type': 'application/x-ndjson' } },
              ),
            )
          : request(input, init)
    },
    { sessionId: detailSessionId },
  )
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  await page.route('**/api/attention', (route) =>
    route.fulfill({ json: { cursor: '0', summaries: [], continuation_after_session_id: null } }),
  )
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: { cursor: '0', summaries: [], continuation_after_session_id: null } })}\n`,
    }),
  )
  await page.route(`**/api/sessions/${detailSessionId}**`, (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/live')) return route.fulfill({ json: detailLive })
    if (url.pathname.endsWith('/timeline')) return route.fulfill({ json: timelineWindow })
    if (url.pathname.endsWith('/timeline-detail')) {
      reads.push(url)
      if (url.searchParams.get('first') !== url.searchParams.get('through')) {
        if (url.searchParams.get('cursor_field') === 'tool_result') {
          let item = toolResultItem()
          const prefix = '{"status":"ok"}'
          const fullText = `${prefix}${' '.repeat(40)}`
          const offset = Number(url.searchParams.get('cursor_offset'))
          const next = {
            ...resultCursor.body,
            offset_bytes: String(prefix.length),
          }
          if (partialResult && item.body.type === 'tool_batch') {
            item = {
              ...item,
              projected_body_bytes: 128 + (offset === 0 ? prefix.length : fullText.length - offset),
              body: {
                ...item.body,
                tools: item.body.tools.map((tool) => ({
                  ...tool,
                  evidence:
                    tool.evidence.type === 'physical_attempt'
                      ? {
                          ...tool.evidence,
                          result: {
                            text: offset === 0 ? prefix : fullText.slice(offset),
                            offset_bytes: String(offset),
                            total_bytes: String(fullText.length),
                            continuation: offset === 0 ? next : null,
                          },
                        }
                      : tool.evidence,
                })),
              },
            }
          }
          return route.fulfill({
            json: detailPage(
              [item],
              partialResult && offset === 0
                ? { type: 'more_body', body: next }
                : { type: 'more_at', address: { event_sequence: '3' } },
            ),
          })
        }
        if (url.searchParams.get('cursor_address') === '3')
          return route.fulfill({ json: detailPage(items.slice(2)) })
        return route.fulfill({ json: detailPage(items.slice(0, 2), resultCursor) })
      }
      let selected = items.find(
        (item) => item.address.event_sequence === url.searchParams.get('first'),
      )
      if (!selected)
        return route.fulfill({
          status: 404,
          json: { error: { code: 'missing', message: 'Fixture detail missing' } },
        })
      if (override && selected.body.type === 'tool_approval_decision') {
        selected = {
          ...selected,
          body: {
            ...selected.body,
            decision: 'approve',
            actor: {
              type: 'user_override',
              command_id: '00000000-0000-0000-0000-000000000123',
              denied_request_id: '00000000-0000-0000-0000-000000000124',
            },
          },
        }
      }
      const terminal = items[4]
      if (mismatch && selected.kind === 'tool_batch_transition' && terminal)
        return route.fulfill({
          json: detailPage([{ ...terminal, address: selected.address }]),
        })
      if (url.searchParams.get('cursor_field') === 'tool_result')
        return route.fulfill({ json: detailPage([toolResultItem()]) })
      return route.fulfill({
        json: detailPage(
          [selected],
          selected.kind === 'tool_batch_transition' ? resultCursor : null,
        ),
      })
    }
    return route.fulfill({
      json: {
        session_id: detailSessionId,
        first_address: { event_sequence: '1' },
        latest_address: { event_sequence: '5' },
        observed_through: '5',
        sizes: {
          item_count: '5',
          projected_text_bytes: '256',
          projected_structured_bytes: String(timelineWindow.projected_structured_bytes),
          referenced_blob_count: '0',
          referenced_blob_bytes: '0',
        },
        work: { active_turn_count: '0', queued_turn_count: '0' },
      },
    })
  })
  await page.goto('/sessions?workspace=true')
  await page.getByRole('textbox', { name: 'Exact session ID' }).fill(detailSessionId)
  await page.getByRole('button', { name: 'Open workspace' }).click()
  await expect(page.getByRole('heading', { name: detailSessionId })).toBeVisible()
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  return reads
}

const toolRow = (page: Page) => page.getByRole('row').filter({ hasText: 'tool batch transition' })

test('opens tool arguments and follows the typed result continuation', async ({ page }) => {
  const reads = await openDetails(page)
  await toolRow(page).click()
  await expect(page.getByRole('region', { name: 'Tool arguments' })).toContainText(
    'release status --json',
  )
  const timeline = page.getByRole('grid', { name: 'Session timeline' })
  expect(await timeline.ariaSnapshot()).toContain('button "Load next detail chunk"')
  await page.getByRole('button', { name: 'Load next detail chunk', exact: true }).press('Enter')
  await expect(page.getByRole('region', { name: 'Tool result' })).toContainText('"checks":"passed"')
  expect(reads.at(-1)?.searchParams.get('cursor_field')).toBe('tool_result')
  expect(reads.at(-1)?.searchParams.get('cursor_member')).toBe('0')
  await expect(page.getByRole('region', { name: 'Tool arguments' })).toHaveCount(0)
  await expect(toolRow(page)).toContainText('Tool result')
  expect(await timeline.ariaSnapshot()).toContain('button "Return to event"')
  await expect(page.getByRole('button', { name: 'Return to event' })).toBeFocused()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('grid', { name: 'Session timeline' })).toBeFocused()
})

test('fails closed when detail belongs to a different event kind', async ({ page }) => {
  await openDetails(page, true)
  await toolRow(page).click()
  await expect(page.getByRole('alert').filter({ hasText: 'Detail rejected' })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Tool arguments' })).toHaveCount(0)
})

test('opens approval and provider detail independently with the keyboard', async ({ page }) => {
  await openDetails(page)
  await page.getByRole('row').filter({ hasText: 'tool approval decided' }).press('Enter')
  await expect(page.getByRole('region', { name: 'Approval rationale' })).toContainText(
    'operator decision',
  )
  await page.getByRole('row').filter({ hasText: 'model call transition' }).press('Enter')
  await expect(page.getByRole('region', { name: 'Model response' })).toContainText('checks passed')
  await expect(page.getByRole('region', { name: 'Approval rationale' })).toBeVisible()
})

test('correlates an override approval with the denied request', async ({ page }) => {
  await openDetails(page, false, true)
  await page.getByRole('row').filter({ hasText: 'tool approval decided' }).press('Enter')
  const detail = page.getByRole('article', {
    name: 'tool approval decided detail',
  })
  await expect(detail).toContainText('user override')
  await expect(detail).toContainText('Command00000000-0000-0000-0000-000000000123')
  await expect(detail).toContainText('Denied request00000000-0000-0000-0000-000000000124')
})

test('captures sessions detail evidence', async ({ page, browserName }) => {
  test.skip(browserName !== 'chromium', 'Chromium owns pixel evidence')
  await page.setViewportSize({ width: 1440, height: 1200 })
  await openDetails(page)
  await toolRow(page).click()
  await page.getByRole('button', { name: 'Load next detail chunk', exact: true }).click()
  await expect(page.getByRole('region', { name: 'Tool result' })).toBeVisible()
  await expect.soft(page).toHaveScreenshot('sessions-detail-desktop-dark.png')
  await page.getByRole('row').filter({ hasText: 'tool approval decided' }).click()
  await page.getByRole('region', { name: 'Approval rationale' }).scrollIntoViewIfNeeded()
  await expect(page.getByRole('region', { name: 'Approval rationale' })).toBeVisible()
  await page.getByRole('button', { name: 'Use light theme' }).click()
  await expect.soft(page).toHaveScreenshot('sessions-detail-desktop-light.png')
  await page.setViewportSize({ width: 390, height: 844 })
  await page.getByRole('region', { name: 'Approval rationale' }).scrollIntoViewIfNeeded()
  await expect.soft(page).toHaveScreenshot('sessions-detail-mobile-light.png')
})

for (const cause of [
  { type: 'interactive' },
  { type: 'repository_watch', dispatch_id: '00000000-0000-0000-0000-000000000551' },
  { type: 'commissioned', dispatch_id: '00000000-0000-0000-0000-000000000552' },
  { type: 'delegated', spawning_request_id: '00000000-0000-0000-0000-000000000553' },
] as const) {
  test(`shows the ${cause.type} creation cause and identity`, async ({ page }) => {
    await openDetails(page, false, false, cause)
    const row = page.getByRole('row').filter({ hasText: 'session created' })
    await row.press('Enter')
    await expect(row).toContainText(cause.type.replaceAll('_', ' '))
    if (cause.type === 'delegated') {
      await expect(row).toContainText('Spawning request')
      await expect(row).toContainText(cause.spawning_request_id)
    } else if (cause.type !== 'interactive') {
      await expect(row).toContainText('Dispatch')
      await expect(row).toContainText(cause.dispatch_id)
    }
  })
}

test('reads tool arguments and output in conversation order with events hidden', async ({
  page,
}) => {
  await openDetails(page)
  await page.getByRole('checkbox', { name: 'Events', exact: true }).uncheck()
  const conversation = page.getByRole('region', { name: 'Conversation', exact: true })
  await expect(page.getByRole('grid', { name: 'Session timeline' })).toBeHidden()
  await expect(conversation.getByRole('region', { name: 'Arguments', exact: true })).toContainText(
    'release status --json',
  )
  await expect(conversation.getByRole('region', { name: 'Output', exact: true })).toContainText(
    'passed',
  )
  await expect(
    conversation.getByText('The release checks passed. Publishing remains unapproved.', {
      exact: true,
    }),
  ).toBeVisible()
  await expect
    .poll(() =>
      conversation
        .locator('[data-event-sequence]')
        .evaluateAll((entries) =>
          entries.map((entry) => entry.getAttribute('data-event-sequence')),
        ),
    )
    .toEqual(['1', '2', '2', '4'])
  await conversation.focus()
  await expect(conversation).toBeFocused()
  await page.getByRole('checkbox', { name: 'Events', exact: true }).check()
  await expect(toolRow(page)).toBeVisible()
})

test('shows reconciliation-required turn outcomes with events hidden', async ({ page }) => {
  await openDetails(page, false, false, undefined, 'reconciliation')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).uncheck()
  const conversation = page.getByRole('region', { name: 'Conversation', exact: true })
  await expect(
    conversation.getByText('Turn reconciliation required · model call', {
      exact: true,
    }),
  ).toBeVisible()
  await expect(page.getByRole('grid', { name: 'Session timeline' })).toBeHidden()
  await page.screenshot({ path: test.info().outputPath('reconciliation-outcome.png') })
})

test('shows retired goal turns in conversation order with events hidden', async ({ page }) => {
  await openDetails(page, false, false, undefined, 'retired')
  await page.getByRole('checkbox', { name: 'Events', exact: true }).uncheck()
  const conversation = page.getByRole('region', { name: 'Conversation', exact: true })
  await expect(conversation.getByText('Goal turn retired', { exact: true })).toBeVisible()
  await expect(page.getByRole('grid', { name: 'Session timeline' })).toBeHidden()
  await expect
    .poll(() =>
      conversation
        .locator('[data-event-sequence]')
        .evaluateAll((entries) =>
          entries.map((entry) => entry.getAttribute('data-event-sequence')),
        ),
    )
    .toEqual(['1', '2', '2', '4', '5'])
  await page.screenshot({ path: test.info().outputPath('retired-outcome.png') })
})

test('labels partial tool payloads and the final continued chunk', async ({ page }) => {
  await openDetails(page, false, false, undefined, undefined, true)
  await page.getByRole('checkbox', { name: 'Events', exact: true }).uncheck()
  const conversation = page.getByRole('region', { name: 'Conversation', exact: true })
  const output = conversation.getByRole('region', { name: 'Output', exact: true })
  await expect(output).toContainText('Text excerpt · byte 0 of 55')
  await expect(output).toContainText('"status": "ok"')
  await expect(
    conversation.getByRole('region', { name: 'Arguments', exact: true }),
  ).not.toContainText('Text excerpt')
  await page.getByRole('button', { name: 'Next text page', exact: true }).click()
  await expect(output).toContainText('Text excerpt · byte 15 of 55')
})
