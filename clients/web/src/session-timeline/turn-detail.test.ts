import { afterEach, expect, it, vi } from 'vitest'
import {
  detailItems,
  detailPage,
  detailSessionId,
  detailTurnId,
  resultCursor,
  toolResultItem,
} from '../../e2e/session-detail-fixture'
import { webContractBootstrapFixture } from '../product.fixture'
import { readTurnTranscript } from './turn-detail'

afterEach(() => vi.unstubAllGlobals())
it('reads the turn route and preserves typed body continuation identity', async () => {
  const first = detailItems[1]
  if (!first) throw new Error('tool fixture missing')
  const request = vi
    .fn()
    .mockResolvedValueOnce(Response.json(detailPage([first], resultCursor)))
    .mockResolvedValueOnce(Response.json(detailPage([toolResultItem()])))
  vi.stubGlobal('fetch', request)
  const initial = await readTurnTranscript(
    detailSessionId,
    detailTurnId,
    { type: 'more_at', address: first.address },
    webContractBootstrapFixture.limits,
  )
  const next = await readTurnTranscript(
    detailSessionId,
    detailTurnId,
    resultCursor,
    webContractBootstrapFixture.limits,
    undefined,
    initial,
  )
  expect(next.items[0]?.body).toMatchObject({
    type: 'tool_batch',
    tools: [{ evidence: { result: { text: '{"release":"ready","checks":"passed"}' } } }],
  })
  expect(request.mock.calls[0]?.[0]).toContain(`/turns/${detailTurnId}/timeline-detail?`)
  expect(request.mock.calls[1]?.[0]).toContain('cursor_field=tool_result')
})

it('rejects detail for another turn', async () => {
  const item = detailItems[0]
  if (item?.body.type !== 'user_input') throw new Error('input fixture missing')
  vi.stubGlobal(
    'fetch',
    vi
      .fn()
      .mockResolvedValue(
        Response.json(detailPage([{ ...item, body: { ...item.body, turn_id: detailSessionId } }])),
      ),
  )
  await expect(
    readTurnTranscript(detailSessionId, detailTurnId, null, webContractBootstrapFixture.limits),
  ).rejects.toThrow('requested turn')
})
