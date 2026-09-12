import { afterEach, expect, it, vi } from 'vitest'
import { webContractBootstrapFixture } from '../product.fixture'
import { TranscriptWindowReader } from './transcript'
import { transcriptFixture, transcriptSessionId } from './transcript.fixture'

afterEach(() => vi.unstubAllGlobals())
it('browses a hundred thousand messages with bounded reads in both directions', async () => {
  const requests: URL[] = []
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) => {
      const url = new URL(path, 'http://localhost')
      requests.push(url)
      return Response.json(transcriptFixture(url))
    }),
  )
  const signal = new AbortController().signal
  const reader = new TranscriptWindowReader(transcriptSessionId)
  const tail = await reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal)
  expect(tail.details).toHaveLength(8)
  expect(tail.window.items.at(-1)?.address.event_sequence).toBe('100000')
  const before = tail.window.continuation_before
  expect(before).not.toBeNull()
  const earlier = await reader.read(
    { kind: 'before', eventSequence: before?.event_sequence ?? '' },
    webContractBootstrapFixture.limits,
    signal,
  )
  expect(earlier.window.items.at(-1)?.address.event_sequence).toBe('99992')
  const after = earlier.window.continuation_after
  const later = await reader.read(
    { kind: 'after', eventSequence: after?.event_sequence ?? '' },
    webContractBootstrapFixture.limits,
    signal,
  )
  expect(later.window.items).toEqual(tail.window.items)
  expect(requests.filter((url) => url.pathname.endsWith('/timeline-detail'))).toHaveLength(24)
})

it('rejects changed immutable kinds on overlapping transcript reads', async () => {
  let conflict = false
  vi.stubGlobal(
    'fetch',
    vi.fn(async (path: string) => {
      const url = new URL(path, 'http://localhost')
      const payload = transcriptFixture(url)
      if (conflict && url.pathname.endsWith('/timeline')) {
        const window = payload as import('../generated/web-contract.mjs').WebSessionTimelineWindow
        return Response.json({
          ...window,
          items: window.items.map((item, index) =>
            index ? item : { ...item, kind: 'turn_completed' },
          ),
        })
      }
      return Response.json(payload)
    }),
  )
  const reader = new TranscriptWindowReader(transcriptSessionId)
  const signal = new AbortController().signal
  await reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal)
  conflict = true
  await expect(
    reader.read({ kind: 'latest' }, webContractBootstrapFixture.limits, signal),
  ).rejects.toThrow('conflicting data for a retained address')
})
