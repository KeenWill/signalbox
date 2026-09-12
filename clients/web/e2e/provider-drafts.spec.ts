import type { WebSessionLiveSnapshot } from '../src/generated/web-contract.mjs'
import { webContractBootstrapFixture } from '../src/product.fixture'
import { expect, test } from './fontTest'

const sessionId = '00000000-0000-0000-0000-000000000991'
const live: WebSessionLiveSnapshot = {
  session_id: sessionId,
  observed_through: '41',
  active: { turn_id: sessionId, state: { kind: 'running', model_call_id: sessionId } },
  queued_turn_count: '0',
  queued_turn_ids: [],
  reconciliation: null,
  runner: {
    state: 'pinned',
    runner_id: sessionId,
    placement_revision: '1',
    connection_health: 'suspect',
  },
}

const replacement: WebSessionLiveSnapshot = {
  ...live,
  observed_through: '42',
  active: null,
  reconciliation: { kind: 'model_call', turn_id: sessionId, model_call_id: sessionId },
}

test('shows bounded provider drafts and live facts, then replaces them on resync', async ({
  page,
}, testInfo) => {
  const problems: string[] = []
  page.on('pageerror', (error) => problems.push(error.message))
  page.on('console', (message) => {
    if (message.type() === 'error') problems.push(message.text())
  })
  await page.addInitScript(
    ({ live, replacement }) => {
      const original = window.fetch
      let follows = 0
      window.fetch = async (input, init) => {
        if (String(input).endsWith(`/sessions/${live.session_id}/follow`)) {
          const snapshot = follows++ === 0 ? live : replacement
          const encoder = new TextEncoder()
          let listener: (event: Event) => void
          return new Response(
            new ReadableStream({
              start(controller) {
                controller.enqueue(
                  encoder.encode(`${JSON.stringify({ kind: 'snapshot', snapshot })}\n`),
                )
                listener = (event) =>
                  controller.enqueue(
                    encoder.encode(`${JSON.stringify((event as CustomEvent).detail)}\n`),
                  )
                window.addEventListener('fixture-live-event', listener)
                Object.assign(window, { fixtureFollowReady: true })
              },
              cancel() {
                window.removeEventListener('fixture-live-event', listener)
              },
            }),
            { headers: { 'content-type': 'application/x-ndjson' } },
          )
        }
        return original(input, init)
      }
    },
    { live, replacement },
  )
  await page.route('**/api/bootstrap', (route) =>
    route.fulfill({ json: webContractBootstrapFixture }),
  )
  const attention = { cursor: '0', summaries: [], continuation_after_session_id: null }
  await page.route('**/api/attention', (route) => route.fulfill({ json: attention }))
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: attention })}\n`,
    }),
  )
  let holdLive = false
  let releaseLive = () => {}
  const heldLive = new Promise<void>((resolve) => {
    releaseLive = resolve
  })
  await page.route(`**/api/sessions/${sessionId}/live`, async (route) => {
    if (holdLive) await heldLive
    await route.fulfill({ json: holdLive ? replacement : live })
  })
  await page.route(`**/api/sessions/${sessionId}/timeline?**`, (route) =>
    route.fulfill({
      json: {
        session_id: sessionId,
        items: [
          {
            address: { event_sequence: '41' },
            kind: 'input_accepted',
            projected_structured_bytes: 78,
          },
        ],
        projected_structured_bytes: 78,
        continuation_before: null,
        continuation_after: null,
      },
    }),
  )
  await page.route(`**/api/sessions/${sessionId}`, (route) =>
    route.fulfill({
      json: {
        session_id: sessionId,
        first_address: { event_sequence: '41' },
        latest_address: { event_sequence: '41' },
        observed_through: holdLive ? '42' : '41',
        work: { active_turn_count: holdLive ? '0' : '1', queued_turn_count: '0' },
        supervision: null,
        repository_watch: null,
        workspace_root_kind: null,
        sizes: {
          item_count: '1',
          projected_text_bytes: '0',
          projected_structured_bytes: '78',
          referenced_blob_count: '0',
          referenced_blob_bytes: '0',
        },
      },
    }),
  )
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`/sessions?workspace=true&session=${sessionId}`)
  await expect(page.getByText('Live', { exact: true })).toBeVisible()
  await page.waitForFunction(() => Reflect.get(window, 'fixtureFollowReady') === true)
  await page.evaluate(
    ({ sessionId }) =>
      window.dispatchEvent(
        new CustomEvent('fixture-live-event', {
          detail: {
            kind: 'provider_text_delta',
            turn_id: sessionId,
            model_call_id: sessionId,
            part_index: 0,
            content: 'I am checking the recorded work before continuing.',
          },
        }),
      ),
    { sessionId },
  )
  await expect(page.getByRole('region', { name: 'Assistant draft' })).toContainText(
    'I am checking the recorded work',
  )
  await page.getByText('Session details', { exact: true }).click()
  await expect(page.getByText('Runner: Assigned, Connection uncertain')).toBeVisible()
  if (testInfo.project.name === 'chromium' && process.platform === 'linux')
    await expect.soft(page).toHaveScreenshot('provider-drafts.png', { animations: 'disabled' })
  holdLive = true
  await page.evaluate(() =>
    window.dispatchEvent(
      new CustomEvent('fixture-live-event', {
        detail: {
          kind: 'resync_required',
          cursor: '41',
        },
      }),
    ),
  )
  await expect(page.getByText('Reconnecting…')).toBeVisible()
  await expect(page.getByRole('region', { name: 'Assistant draft' })).toHaveCount(0)
  await expect(page.getByText('Runner: Assigned, Connection uncertain')).toHaveCount(0)
  releaseLive()
  await expect(page.getByText('Recovery needed · Model call')).toBeVisible()
  await expect(page.getByText('Live', { exact: true })).toBeVisible()
  expect(problems).toEqual([])
})
