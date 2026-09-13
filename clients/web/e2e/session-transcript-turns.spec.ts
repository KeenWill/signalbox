import { transcriptFixture } from '../src/session-timeline/transcript.fixture'
import { turnApi } from '../src/session-timeline/turns.fixture'
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
