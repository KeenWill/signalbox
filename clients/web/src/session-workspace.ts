import type { QueryClient } from '@tanstack/react-query'
import type {
  WebSessionTimelineDescriptor,
  WebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import type { BoundedSessionHistory, SessionWindowAnchor } from './session-timeline/model'

export const SESSION_WINDOW_ITEMS = 80
export const SESSION_WINDOW_BYTES = 64 * 1024

export interface SessionWorkspace {
  active: boolean
  requestedAddress?: string
  anchor: SessionWindowAnchor
  descriptor: WebSessionTimelineDescriptor
  history: BoundedSessionHistory
  window: WebSessionTimelineWindow
}

export async function extendSessionWorkspace(
  queries: QueryClient,
  sessionId: string,
  observed: string,
  signal: AbortSignal,
) {
  const queryKey = ['production', 'session-workspace', sessionId] as const
  try {
    await queries.getQueryCache().find({ queryKey, exact: true })?.promise
  } catch {
    return
  }
  const held = queries.getQueryData<SessionWorkspace>(queryKey)
  if (!held || signal.aborted || BigInt(observed) <= BigInt(held.descriptor.observed_through))
    return
  let descriptor = await held.history.describe(signal)
  const anchor: SessionWindowAnchor =
    held.requestedAddress === undefined &&
    held.anchor.kind === 'around' &&
    (descriptor.work.active_turn_count !== '0' || descriptor.work.queued_turn_count !== '0')
      ? { kind: 'latest' }
      : held.anchor
  let window = held.window
  const last = window.items.at(-1)?.address.event_sequence
  if (
    last &&
    anchor.kind !== 'latest' &&
    BigInt(descriptor.latest_address.event_sequence) > BigInt(last)
  ) {
    window = { ...window, continuation_after: { event_sequence: last } }
  }
  if (
    last &&
    anchor.kind === 'latest' &&
    BigInt(descriptor.latest_address.event_sequence) > BigInt(last)
  ) {
    const extension = await held.history.load(
      { kind: 'after', eventSequence: last },
      { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
      signal,
    )
    if (signal.aborted) return
    const extendedThrough = extension.items.at(-1)?.address.event_sequence ?? last
    if (
      extension.continuation_after !== null ||
      BigInt(extendedThrough) < BigInt(descriptor.latest_address.event_sequence)
    ) {
      await queries.invalidateQueries({ queryKey, exact: true })
      return
    }
    if (BigInt(extendedThrough) > BigInt(descriptor.latest_address.event_sequence)) {
      descriptor = await held.history.describe(signal)
      if (BigInt(extendedThrough) > BigInt(descriptor.latest_address.event_sequence))
        throw new TypeError('timeline extension exceeds the reconciled descriptor')
    }
    const items = [...window.items, ...extension.items]
    let bytes = window.projected_structured_bytes + extension.projected_structured_bytes
    let evicted = false
    while (items.length > SESSION_WINDOW_ITEMS || bytes > SESSION_WINDOW_BYTES) {
      bytes -= items.shift()?.projected_structured_bytes ?? 0
      evicted = true
    }
    window = {
      ...extension,
      items,
      projected_structured_bytes: bytes,
      continuation_before: evicted ? (items[0]?.address ?? null) : window.continuation_before,
    }
  }
  if (signal.aborted) return
  queries.setQueryData<SessionWorkspace>(queryKey, (current) =>
    current === held
      ? {
          ...held,
          anchor,
          descriptor,
          active:
            descriptor.work.active_turn_count !== '0' || descriptor.work.queued_turn_count !== '0',
          window,
        }
      : current,
  )
}
