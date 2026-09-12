import type {
  WebSessionLiveActiveState,
  WebSessionTimelineDescriptor,
  WebSessionTimelineDetail,
} from '../src/generated/web-contract.mjs'
import { webContractBootstrapFixture as bootstrapFixture } from '../src/product.fixture'
import { expect, type Page } from './fontTest'

export type WebRepositoryWatchProvenance = NonNullable<
  WebSessionTimelineDescriptor['repository_watch']
>

export const sessionId = '00000000-0000-0000-0000-000000000991'
export const turnId = '00000000-0000-0000-0000-000000000992'
export const initialMessage = 'Check the session and explain the next step.'
export const assistantMessage =
  'The session is ready. I can continue from the recorded conversation.'
export const excerpt = (text: string) => ({
  text,
  offset_bytes: '0',
  total_bytes: String(new TextEncoder().encode(text).length),
  continuation: null,
})

export async function sessionApi(
  page: Page,
  busy = false,
  selectedSessionId = sessionId,
  origin: WebRepositoryWatchProvenance | null = null,
) {
  const state = {
    supervision: null as WebSessionTimelineDescriptor['supervision'],
    active: busy,
    activeState: { kind: 'running', model_call_id: null } as WebSessionLiveActiveState,
    grown: false,
    observed: false,
    historyReads: [] as string[],
    textReads: [] as string[],
    submissions: [] as Array<{ command_id: string; message: string }>,
  }
  let releaseFollow = () => {}
  const followReady = new Promise<void>((resolve) => {
    releaseFollow = resolve
  })
  const snapshot = () => ({
    session_id: selectedSessionId,
    observed_through: state.grown || state.observed ? '44' : '43',
    active: state.active ? { turn_id: turnId, state: state.activeState } : null,
    queued_turn_count: '0',
    queued_turn_ids: [],
    reconciliation: null,
    runner: null,
  })
  await page.route('**/api/bootstrap', (route) => route.fulfill({ json: bootstrapFixture }))
  await page.route('**/api/attention', (route) =>
    route.fulfill({ json: { cursor: '0', summaries: [], continuation_after_session_id: null } }),
  )
  await page.route('**/api/attention/follow', (route) =>
    route.fulfill({
      contentType: 'application/x-ndjson',
      body: `${JSON.stringify({ kind: 'snapshot', snapshot: { cursor: '0', summaries: [], continuation_after_session_id: null } })}\n`,
    }),
  )
  await page.route(`**/api/sessions/${selectedSessionId}**`, async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/input')) {
      state.submissions.push(route.request().postDataJSON())
      return route.fulfill({ status: 204 })
    }
    if (url.pathname.endsWith('/live')) return route.fulfill({ json: snapshot() })
    if (url.pathname.endsWith('/follow')) {
      const initial = snapshot()
      await followReady
      return route.fulfill({
        contentType: 'application/x-ndjson',
        body: `${JSON.stringify({ kind: 'snapshot', snapshot: initial })}\n${JSON.stringify({ kind: 'durable', cursor: '44', address: { event_sequence: '44' }, event_kind: 'model_call_transition' })}\n`,
      })
    }
    if (url.pathname.endsWith('/timeline-detail')) {
      state.textReads.push(url.searchParams.get('first') ?? '')
      const items: WebSessionTimelineDetail[] = [
        {
          address: { event_sequence: '41' },
          kind: 'input_accepted',
          projected_body_bytes: 128 + initialMessage.length,
          body: {
            type: 'user_input',
            turn_id: turnId,
            text: excerpt(initialMessage),
            attachments: [],
          },
        },
      ]
      if (state.grown)
        items.push({
          address: { event_sequence: '44' },
          kind: 'model_call_transition',
          projected_body_bytes: 128 + assistantMessage.length,
          body: {
            type: 'model_call',
            turn_id: turnId,
            model_call_id: turnId,
            model_identity_id: turnId,
            request_context_items: '1',
            response: excerpt(assistantMessage),
            state: { type: 'terminal', disposition: 'completed' },
            usage: {},
          },
        })
      const selected = items.filter(
        (item) =>
          BigInt(item.address.event_sequence) >= BigInt(url.searchParams.get('first') ?? '0'),
      )
      return route.fulfill({
        json: {
          session_id: selectedSessionId,
          projected_body_bytes: selected.reduce((sum, item) => sum + item.projected_body_bytes, 0),
          items: selected,
          continuation: null,
        },
      })
    }
    const latest = state.grown ? '44' : '43'
    if (url.pathname.endsWith('/timeline')) {
      state.historyReads.push(url.searchParams.get('anchor') ?? '')
      return route.fulfill({
        json: {
          session_id: selectedSessionId,
          items: [
            {
              address: { event_sequence: '41' },
              kind: 'input_accepted',
              projected_structured_bytes: 78,
            },
            {
              address: { event_sequence: '43' },
              kind: 'turn_completed',
              projected_structured_bytes: 78,
            },
            ...(state.grown
              ? [
                  {
                    address: { event_sequence: '44' },
                    kind: 'model_call_transition',
                    projected_structured_bytes: 85,
                  },
                ]
              : []),
          ].filter(
            (item) =>
              url.searchParams.get('anchor') !== 'after' ||
              BigInt(item.address.event_sequence) > BigInt(url.searchParams.get('address') ?? '0'),
          ),
          projected_structured_bytes:
            url.searchParams.get('anchor') === 'after' ? 85 : state.grown ? 241 : 156,
          continuation_before:
            url.searchParams.get('anchor') === 'after' ? { event_sequence: '44' } : null,
          continuation_after: null,
        },
      })
    }
    return route.fulfill({
      json: {
        session_id: selectedSessionId,
        supervision: state.supervision,
        repository_watch: origin,
        workspace_root_kind: null,
        sizes: {
          item_count: state.grown ? '3' : '2',
          projected_text_bytes: String(
            initialMessage.length + (state.grown ? assistantMessage.length : 0),
          ),
          projected_structured_bytes: state.grown ? '241' : '156',
          referenced_blob_count: '0',
          referenced_blob_bytes: '0',
        },
        first_address: { event_sequence: '41' },
        latest_address: { event_sequence: latest },
        observed_through: state.observed ? '44' : latest,
        work: { active_turn_count: state.active ? '1' : '0', queued_turn_count: '0' },
      },
    })
  })
  return {
    state,
    advanceObservation: () => {
      state.observed = true
      releaseFollow()
    },
    grow: () => {
      state.grown = true
      releaseFollow()
    },
  }
}

export async function openSession(page: Page) {
  await page.goto(`/sessions?workspace=true&session=${sessionId}`)
  await expect(page.getByText(initialMessage, { exact: true })).toBeVisible()
  await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toBeInViewport()
  await expect(page.getByRole('button', { name: 'Send message', exact: true })).toBeInViewport()
}

export async function openSessionFromCatalog(page: Page, id: string) {
  const title = `Fixture session ${id}`
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({
      json: {
        continuation: null,
        cursor: '1',
        sort: 'last_activity_descending',
        total: '1',
        summaries: [
          {
            session_id: id,
            title_summary: title,
            title_truncated: false,
            action: null,
            active_turn_count: '0',
            queued_turn_count: '0',
            archived: false,
            current_turn_id: null,
            goal_block: null,
            state: 'idle',
            judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
            last_activity: { kind: 'session', unix_microseconds: '1724200000000000' },
          },
        ],
      },
    }),
  )
  await page.route('**/api/sessions/rates?**', (route) =>
    route.fulfill({
      json: {
        sessions: [
          {
            session_id: id,
            lifecycle_state: 'created',
            turn_count: '0',
            failed_turn_count: '0',
            retired_turn_count: '0',
            completed_turn_count: '0',
          },
        ],
      },
    }),
  )
  await page.getByRole('link', { name: 'Sessions', exact: true }).click()
  await page.getByRole('button', { name: title }).click()
  await expect.poll(() => new URL(page.url()).searchParams.get('session')).toBe(id)
}
