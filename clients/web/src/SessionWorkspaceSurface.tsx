import {
  type QueryClient,
  useMutation,
  useMutationState,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query'
import { ChevronDown, ChevronRight, SkipBack, SkipForward } from 'lucide-react'
import {
  type KeyboardEvent,
  type ReactNode,
  type RefObject,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import { type SessionAction, submitSessionAction } from './attention'
import { invokeCommand } from './commands'
import { Field } from './Field'
import type { WebSessionTimelineWindow, WebUsageSummary } from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import './session-header.css'
import './session-polish.css'
import { ProductInputError, ProductRequestError, type SessionTranscriptLimits } from './product'
import './session-actions.css'
import { SessionComposer } from './SessionComposer'
import { SessionItemDetail } from './SessionItemDetail'
import { SessionTranscriptText } from './SessionTranscriptText'
import type { SearchUsageSource } from './search-usage/model'
import {
  BoundedSessionHistory,
  HttpSessionTimelineSource,
  type SessionWindowAnchor,
} from './session-timeline/model'
import { SESSION_WINDOW_BYTES, SESSION_WINDOW_ITEMS } from './session-workspace'
import {
  actions,
  MAX_PENDING_SESSION_INPUTS,
  selectApp,
  selectSessionSync,
  store,
  useAppDispatch,
  useAppSelector,
} from './state'

const SESSION_ID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
const MAX_CACHED_SESSION_WORKSPACES = 4
type TimelineCapability = 'checking' | 'available' | 'unavailable'

function createSessionActionCommandId() {
  // UUID v4 uses 16 random bytes with the version and variant bits fixed.
  const bytes = crypto.getRandomValues(new Uint8Array(16))
  const hex = Array.from(bytes, (byte, index) => {
    const value = index === 6 ? (byte & 0x0f) | 0x40 : index === 8 ? (byte & 0x3f) | 0x80 : byte
    return value.toString(16).padStart(2, '0')
  }).join('')
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`
}

function SessionActions({
  sessionId,
  pendingRequest,
  activeTurn,
  onAccepted,
  renderHeader,
}: {
  sessionId: string
  pendingRequest: string | null
  activeTurn: string | null
  onAccepted: () => Promise<unknown>
  renderHeader: (controls: ReactNode) => ReactNode
}) {
  const [draftChoice, setChoice] = useState<
    'approve' | 'deny' | 'cancel' | 'set-goal' | 'clear-goal' | null
  >(null)
  const [text, setText] = useState('')
  const [chosenRequest, setChosenRequest] = useState<string | null>(null)
  const [chosenTurn, setChosenTurn] = useState<string | null>(null)
  const inFlight = useRef(false)
  const actionOpener = useRef<HTMLButtonElement>(null)
  const queryClient = useQueryClient()
  const pendingActions = useMutationState({
    filters: { mutationKey: ['session-action'] },
    select: (mutation) => ({
      sessionId: mutation.options.mutationKey?.[1],
      action: mutation.state.variables as SessionAction,
      status: mutation.state.status,
      error: mutation.state.error,
    }),
  })
  const pending = pendingActions.find((action) => action.sessionId === sessionId)
  const retained = pending?.action ?? null
  const choice = retained
    ? retained.kind === 'approval'
      ? retained.input.decision
      : retained.kind
    : draftChoice
  const retainedText =
    retained?.kind === 'cancel'
      ? retained.input.message
      : retained?.kind === 'set-goal'
        ? retained.input.statement
        : retained?.kind === 'approval'
          ? (retained.input.note ?? '')
          : ''
  const sending = pending?.status === 'pending'
  const capacityReached = retained === null && pendingActions.length >= MAX_PENDING_SESSION_INPUTS
  const [notice, setNotice] = useState('')
  const mutation = useMutation({
    mutationKey: ['session-action', sessionId],
    // Unconfirmed commands remain available across workspace navigation. Confirmed entries are removed below.
    gcTime: Number.POSITIVE_INFINITY,
    mutationFn: (action: SessionAction) => submitSessionAction(sessionId, action),
    onSuccess: () => {
      setChoice(null)
      setText('')
      setNotice('Action accepted')
      void onAccepted()
    },
    onSettled: (_, error, action) => {
      inFlight.current = false
      if (
        !error ||
        error instanceof ProductInputError ||
        (error instanceof ProductRequestError && error.status < 500)
      ) {
        if (error) {
          // A definitive refusal releases the command identity, but keeps an editable draft.
          setChoice(action.kind === 'approval' ? action.input.decision : action.kind)
          setText(
            action.kind === 'cancel'
              ? action.input.message
              : action.kind === 'set-goal'
                ? action.input.statement
                : action.kind === 'approval'
                  ? (action.input.note ?? '')
                  : '',
          )
          setChosenRequest(action.kind === 'approval' ? action.requestId : null)
          setChosenTurn(action.kind === 'cancel' ? action.input.expected_active_turn_id : null)
        }
        for (const mutation of queryClient
          .getMutationCache()
          .findAll({ mutationKey: ['session-action', sessionId] })) {
          if (
            (mutation.state.variables as SessionAction).input.command_id === action.input.command_id
          ) {
            queryClient.getMutationCache().remove(mutation)
          }
        }
      }
    },
  })
  const error = pending?.error ?? mutation.error
  const choose = (next: typeof choice, opener?: HTMLButtonElement) => {
    if (opener) actionOpener.current = opener
    setChoice(next)
    setChosenRequest(pendingRequest)
    setChosenTurn(activeTurn)
    setText('')
    setNotice('')
    mutation.reset()
  }
  const dismiss = () => {
    choose(null)
    actionOpener.current?.focus()
  }
  const confirm = () => {
    if (inFlight.current || sending || capacityReached) return
    let action = retained
    if (!action) {
      const command_id = createSessionActionCommandId()
      if ((choice === 'approve' || choice === 'deny') && chosenRequest) {
        action = {
          kind: 'approval',
          requestId: chosenRequest,
          input: {
            command_id,
            decision: choice,
            // ToolDenialReason preserves non-POSIX whitespace, including NBSP.
            note: choice === 'deny' && /[^ \t\n\v\f\r]/.test(text) ? text : null,
          },
        }
      } else if (choice === 'cancel' && chosenTurn && text.length > 0) {
        action = {
          kind: 'cancel',
          input: { command_id, expected_active_turn_id: chosenTurn, message: text },
        }
      } else if (choice === 'set-goal' && text.length > 0) {
        action = { kind: 'set-goal', input: { command_id, statement: text } }
      } else if (choice === 'clear-goal') {
        action = { kind: 'clear-goal', input: { command_id } }
      }
    }
    if (!action) return
    inFlight.current = true
    for (const previous of queryClient
      .getMutationCache()
      .findAll({ mutationKey: ['session-action', sessionId] })) {
      queryClient.getMutationCache().remove(previous)
    }
    mutation.mutate(action)
  }
  return (
    <section className="session-actions" aria-label="Session actions">
      {renderHeader(
        <div className="session-action-buttons">
          {pendingRequest && (
            <>
              <span className="sr-only">Approval needed</span>
              <button
                type="button"
                disabled={retained !== null || capacityReached}
                onClick={(event) => choose('approve', event.currentTarget)}
              >
                Approve
              </button>
              <button
                type="button"
                disabled={retained !== null || capacityReached}
                onClick={(event) => choose('deny', event.currentTarget)}
              >
                Deny
              </button>
            </>
          )}
          {activeTurn && (
            <button
              type="button"
              disabled={retained !== null || capacityReached}
              onClick={(event) => choose('cancel', event.currentTarget)}
            >
              Cancel turn
            </button>
          )}
          <button
            type="button"
            disabled={retained !== null || capacityReached}
            onClick={(event) => choose('set-goal', event.currentTarget)}
          >
            Set goal
          </button>
          {!pendingRequest && (
            <button
              type="button"
              disabled={retained !== null || capacityReached}
              onClick={(event) => choose('clear-goal', event.currentTarget)}
            >
              Clear goal
            </button>
          )}
        </div>,
      )}
      {choice && (
        <form
          className="session-action-confirmation"
          onKeyDown={(event) => {
            if (event.key !== 'Escape' || retained || inFlight.current) return
            event.preventDefault()
            event.stopPropagation()
            dismiss()
          }}
          onSubmit={(event) => {
            event.preventDefault()
            confirm()
          }}
        >
          {choice === 'set-goal' || choice === 'deny' || choice === 'cancel' ? (
            <div className="session-action-entry">
              <Field
                as="textarea"
                label={
                  choice === 'set-goal'
                    ? 'Goal'
                    : choice === 'cancel'
                      ? 'Message to continue with'
                      : 'Note (optional)'
                }
                rows={3}
                value={retained ? retainedText : text}
                onChange={(event) => setText(event.target.value)}
                disabled={retained !== null || capacityReached}
                required={choice === 'set-goal' || choice === 'cancel'}
              />
              {choice === 'cancel' && <p>Cancel this turn and continue with your message.</p>}
            </div>
          ) : (
            <span>
              {choice === 'approve'
                ? 'Approve this pending request?'
                : 'Clear the goal and stop its current turn?'}
            </span>
          )}
          <button
            type="submit"
            disabled={
              sending ||
              capacityReached ||
              (!retained && (choice === 'set-goal' || choice === 'cancel') && text.length === 0)
            }
          >
            {sending ? 'Sending…' : retained ? 'Retry same action' : 'Confirm'}
          </button>
          {!retained && (
            <button type="button" onClick={dismiss}>
              Keep unchanged
            </button>
          )}
        </form>
      )}
      {capacityReached && (
        <p role="status">Resolve an unconfirmed session action before starting another.</p>
      )}
      {error && (
        <p role="alert">
          {error instanceof ProductRequestError
            ? `${error.response.error.code}: ${error.message}`
            : error instanceof ProductInputError
              ? error.message
              : 'Outcome unconfirmed. Retry the same action.'}
          {retained && <small>Command {retained.input.command_id}</small>}
        </p>
      )}
      {notice && <p role="status">{notice}</p>}
    </section>
  )
}

export const isCanonicalSessionId = (value: string): boolean => SESSION_ID_PATTERN.test(value)
export const sessionWorkspaceQueryKey = (sessionId: string | null) =>
  ['production', 'session-workspace', sessionId] as const

export const evictInactiveSessionWorkspaceQueries = (
  queryClient: QueryClient,
  retainedSessionId: string | null,
): void => {
  const inactiveQueries = queryClient
    .getQueryCache()
    .findAll({ queryKey: ['production', 'session-workspace'] })
    .filter(
      (query) =>
        typeof query.queryKey[2] === 'string' &&
        query.queryKey[2] !== retainedSessionId &&
        query.getObserversCount() === 0,
    )
    .sort((left, right) => right.state.dataUpdatedAt - left.state.dataUpdatedAt)

  for (const query of inactiveQueries.slice(MAX_CACHED_SESSION_WORKSPACES - 1)) {
    queryClient.removeQueries({ queryKey: query.queryKey, exact: true })
  }
}

export const sessionHasLiveWork = (activeTurnCount: string, queuedTurnCount: string): boolean =>
  BigInt(activeTurnCount) !== BigInt(0) || BigInt(queuedTurnCount) !== BigInt(0)

export const reconcileVisibleSessionSelection = (
  selected: string | null,
  visibleIds: readonly string[],
  preferred: string | null = null,
): string | null =>
  selected !== null && visibleIds.includes(selected)
    ? selected
    : preferred !== null && visibleIds.includes(preferred)
      ? preferred
      : (visibleIds[0] ?? null)

export const visibleSessionItems = (
  items: WebSessionTimelineWindow['items'],
  detail: 'full' | 'condensed' | 'results',
  requestedAddress?: string,
) =>
  detail === 'results'
    ? items.filter(
        (item) =>
          item.address.event_sequence === requestedAddress ||
          [
            'input_accepted',
            'turn_completed',
            'turn_failed',
            'turn_refused',
            'turn_cancelled',
            'turn_reconciliation_required',
            'goal_turn_retired',
          ].includes(item.kind),
      )
    : items

export const boundarySessionItemId = (
  items: WebSessionTimelineWindow['items'],
  detail: 'full' | 'condensed' | 'results',
  anchor: 'first' | 'latest',
): string | null => {
  const visible = visibleSessionItems(items, detail)
  const boundary = anchor === 'first' ? visible[0] : visible[visible.length - 1]
  return boundary?.address.event_sequence ?? null
}

export const pruneExpandedSessionItems = (
  expanded: ReadonlySet<string>,
  items: WebSessionTimelineWindow['items'],
): ReadonlySet<string> => {
  const loadedIds = new Set(items.map((item) => item.address.event_sequence))
  const next = new Set([...expanded].filter((id) => loadedIds.has(id)))
  return next.size === expanded.size ? expanded : next
}

const sessionCostLabel = (summary: WebUsageSummary): string => {
  if (summary.truncated) return 'Cost incomplete'
  const totals = { real: 0n, metered_equivalent: 0n }
  const labels = new Set<string>()
  let scale = 0
  for (const group of summary.groups) {
    if (group.cost.status === 'unavailable') return 'Cost unavailable'
    const [whole, fraction = ''] = group.cost.amount_usd.split('.')
    if (fraction.length > scale) {
      totals.real *= 10n ** BigInt(fraction.length - scale)
      totals.metered_equivalent *= 10n ** BigInt(fraction.length - scale)
      scale = fraction.length
    }
    totals[group.cost.label] +=
      BigInt(`${whole}${fraction}`) * 10n ** BigInt(scale - fraction.length)
    labels.add(group.cost.label)
  }
  const divisor = 10n ** BigInt(Math.max(scale - 2, 0))
  const dollars = (total: bigint): string => {
    const cents = scale > 2 ? (total + divisor / 2n) / divisor : total * 10n ** BigInt(2 - scale)
    return `$${new Intl.NumberFormat('en-US').format(cents / 100n)}.${(cents % 100n).toString().padStart(2, '0')}`
  }
  const parts = []
  if (labels.has('real') || labels.size === 0) parts.push(dollars(totals.real))
  if (labels.has('metered_equivalent'))
    parts.push(`${dollars(totals.metered_equivalent)} equivalent`)
  return parts.join(' + ')
}

export function SessionWorkspaceSurface({
  usageSource,
  initialSessionId,
  initialAround,
  onAroundConsumed,
  onReturnToCatalog,
  registerTranscriptUnwind,
  onTimelineIds,
  onTimelineWindowAvailable,
  onWindowRequestConsumed,
  timelineCapability,
  transcriptAvailable,
  transcriptLimits,
  timelineRef,
  windowRequest,
}: {
  usageSource: Pick<SearchUsageSource, 'usageSummary'>
  initialSessionId?: string
  initialAround?: string
  onAroundConsumed: () => void
  focusEntry: boolean
  onSessionOpen: (sessionId: string) => void
  onReturnToCatalog: () => void
  registerTranscriptUnwind?: (handler: () => boolean) => () => void
  onTimelineIds: (ids: readonly string[]) => void
  onTimelineWindowAvailable: (available: boolean) => void
  onWindowRequestConsumed: () => void
  timelineCapability: TimelineCapability
  transcriptAvailable: boolean
  transcriptLimits: SessionTranscriptLimits
  timelineRef: RefObject<HTMLDivElement | null>
  windowRequest: { anchor: 'first' | 'latest'; attempt: number } | null
}) {
  const dispatch = useAppDispatch()
  const queryClient = useQueryClient()
  const app = useAppSelector(selectApp)
  const sessionId = initialSessionId ?? null
  const [awaitingSessionId, setAwaitingSessionId] = useState<string | null>(
    initialSessionId ?? null,
  )
  const [openingPosition] = useState<string | undefined>(
    initialSessionId === undefined ? undefined : app.lastLogicalPositions[initialSessionId],
  )
  const [refetchRequest, setRefetchRequest] = useState(0)
  const [showEvents, setShowEvents] = useState(initialAround !== undefined)
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const workspaceRef = useRef<HTMLElement>(null)
  const detailsRef = useRef<HTMLDetailsElement>(null)
  const rowRefs = useRef(new Map<string, HTMLDivElement>())
  const eventWindowPending = useRef(false)
  const eventScrollOffset = useRef(0)
  const restoredEventOffset = useRef<number | null>(null)
  const eventTouchPosition = useRef<number | null>(null)
  const manualAnchorRef = useRef<SessionWindowAnchor | null>(
    initialAround ? { kind: 'around', eventSequence: initialAround } : null,
  )
  const requestedSelection = useRef(initialAround)
  const previousAround = useRef(initialAround)
  useEffect(() => {
    if (previousAround.current === initialAround) return
    previousAround.current = initialAround
    if (initialAround === undefined) return
    manualAnchorRef.current = { kind: 'around', eventSequence: initialAround }
    requestedSelection.current = initialAround
    setShowEvents(true)
    setAwaitingSessionId(sessionId)
    setRefetchRequest((current) => current + 1)
  }, [initialAround, sessionId])
  const handledRefetchRequest = useRef(0)
  const boundaryRequest = useRef(0)
  const session = useQuery({
    queryKey: sessionWorkspaceQueryKey(sessionId),
    queryFn: async ({ signal }) => {
      const requestedSessionId = sessionId ?? ''
      const cached = queryClient.getQueryData<{ history: BoundedSessionHistory }>(
        sessionWorkspaceQueryKey(requestedSessionId),
      )
      const history =
        cached?.history ??
        new BoundedSessionHistory(
          requestedSessionId,
          await HttpSessionTimelineSource.connect(window.fetch.bind(window), signal),
        )
      const descriptor = await history.describe(signal)
      const active = sessionHasLiveWork(
        descriptor.work.active_turn_count,
        descriptor.work.queued_turn_count,
      )
      let anchor: SessionWindowAnchor =
        manualAnchorRef.current ??
        (!active && openingPosition
          ? { kind: 'around', eventSequence: openingPosition }
          : { kind: 'latest' })
      let timelineWindow = await history.load(
        anchor,
        { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
        signal,
      )
      let reconciledDescriptor = await history.describe(signal)
      let reconciledActive = sessionHasLiveWork(
        reconciledDescriptor.work.active_turn_count,
        reconciledDescriptor.work.queued_turn_count,
      )
      if (manualAnchorRef.current === null && anchor.kind === 'around' && reconciledActive) {
        anchor = { kind: 'latest' }
        timelineWindow = await history.load(
          anchor,
          { maxItems: SESSION_WINDOW_ITEMS, maxBytes: SESSION_WINDOW_BYTES },
          signal,
        )
        reconciledDescriptor = await history.describe(signal)
        reconciledActive = sessionHasLiveWork(
          reconciledDescriptor.work.active_turn_count,
          reconciledDescriptor.work.queued_turn_count,
        )
      }
      const latestWindowAddress = timelineWindow.items.at(-1)?.address.event_sequence
      if (
        latestWindowAddress !== undefined &&
        BigInt(latestWindowAddress) > BigInt(reconciledDescriptor.latest_address.event_sequence)
      ) {
        throw new TypeError('timeline window exceeds the reconciled descriptor')
      }
      return {
        active: reconciledActive,
        requestedAddress: manualAnchorRef.current?.kind === 'around' ? initialAround : undefined,
        anchor,
        descriptor: reconciledDescriptor,
        history,
        window: timelineWindow,
      }
    },
    enabled: sessionId !== null && timelineCapability === 'available',
  })
  const refetchSession = session.refetch
  useEffect(() => {
    if (
      refetchRequest === handledRefetchRequest.current ||
      sessionId === null ||
      timelineCapability !== 'available'
    )
      return
    handledRefetchRequest.current = refetchRequest
    void refetchSession()
  }, [refetchRequest, refetchSession, sessionId, timelineCapability])
  const synchronization = useAppSelector(selectSessionSync)
  const live = synchronization.sessionId === sessionId ? synchronization.snapshot : null
  const followFailed = synchronization.sessionId === sessionId && synchronization.phase === 'failed'
  useEffect(() => {
    dispatch(actions.sessionFollowRequested(timelineCapability === 'available' ? sessionId : null))
    return () => {
      dispatch(actions.sessionFollowRequested(null))
    }
  }, [dispatch, sessionId, timelineCapability])
  const displayedSession =
    session.isSuccess && awaitingSessionId !== sessionId ? session.data : undefined
  const timelineCostPosition = displayedSession?.descriptor.observed_through
  const liveCostPosition = synchronization.sessionId === sessionId ? synchronization.cursor : null
  const costPosition =
    timelineCostPosition !== undefined &&
    liveCostPosition !== null &&
    BigInt(liveCostPosition) > BigInt(timelineCostPosition)
      ? liveCostPosition
      : timelineCostPosition
  const observedCostPosition = useRef<string | undefined>(undefined)
  const cost = useQuery({
    queryKey: ['production', 'session-cost', sessionId],
    queryFn: async ({ signal }) => {
      observedCostPosition.current = costPosition
      return usageSource.usageSummary({ sessionId: sessionId ?? '' }, signal)
    },
    enabled: displayedSession !== undefined,
    gcTime: 0,
  })
  const costLabel = cost.isError
    ? 'Cost unavailable'
    : cost.data
      ? sessionCostLabel(cost.data)
      : 'Cost loading…'
  const refetchCost = cost.refetch
  useEffect(() => {
    if (
      costPosition !== undefined &&
      !cost.isFetching &&
      observedCostPosition.current !== costPosition
    ) {
      void refetchCost({ cancelRefetch: false })
    }
  }, [costPosition, cost.isFetching, refetchCost])
  const origin = displayedSession?.descriptor.repository_watch
  const items = useMemo(
    () => visibleSessionItems(displayedSession?.window.items ?? [], app.detail, initialAround),
    [app.detail, displayedSession?.window.items, initialAround],
  )
  const timelineIds = useMemo(() => items.map((item) => item.address.event_sequence), [items])
  const loadWindow = useCallback(
    async (anchor: 'first' | 'latest', control?: HTMLButtonElement) => {
      const request = ++boundaryRequest.current
      manualAnchorRef.current = { kind: anchor }
      requestedSelection.current = undefined
      onAroundConsumed()
      const result = await refetchSession()
      if (!result.isSuccess || result.data === undefined || request !== boundaryRequest.current) {
        return
      }
      dispatch(
        actions.timelineSelected(
          boundarySessionItemId(result.data.window.items, store.getState().app.detail, anchor),
        ),
      )
      if (control === undefined) timelineRef.current?.focus()
    },
    [dispatch, onAroundConsumed, refetchSession, timelineRef],
  )
  const loadEventNeighbor = async (direction: 'before' | 'after') => {
    if (!showEvents || session.isFetching || eventWindowPending.current) return
    const continuation =
      direction === 'before'
        ? displayedSession?.window.continuation_before
        : displayedSession?.window.continuation_after
    if (!continuation) return
    eventWindowPending.current = true
    ++boundaryRequest.current
    manualAnchorRef.current = { kind: direction, eventSequence: continuation.event_sequence }
    requestedSelection.current = undefined
    onAroundConsumed()
    try {
      await refetchSession()
    } finally {
      eventWindowPending.current = false
    }
  }
  const traverseEventEdge = (element: HTMLDivElement, direction: 'before' | 'after') => {
    const remaining =
      direction === 'before'
        ? element.scrollTop
        : element.scrollHeight - element.clientHeight - element.scrollTop
    if (remaining <= 1) void loadEventNeighbor(direction)
  }
  useLayoutEffect(() => {
    const element = timelineRef.current
    if (!showEvents || !element) return
    if (displayedSession?.anchor.kind === 'before') element.scrollTop = element.scrollHeight
    if (displayedSession?.anchor.kind === 'after') element.scrollTop = 0
    eventScrollOffset.current = element.scrollTop
    restoredEventOffset.current = element.scrollTop
  }, [displayedSession?.anchor, showEvents, timelineRef])
  const toggleSelectedExpansion = useCallback(() => {
    const eventSequence = store.getState().app.selectedTimeline
    if (eventSequence === null || !timelineIds.includes(eventSequence)) return
    setExpanded((current) => {
      const next = new Set(current)
      if (next.has(eventSequence)) next.delete(eventSequence)
      else next.add(eventSequence)
      return next
    })
  }, [timelineIds])

  useEffect(
    () => onTimelineIds(showEvents ? timelineIds : []),
    [onTimelineIds, timelineIds, showEvents],
  )
  useEffect(() => () => onTimelineIds([]), [onTimelineIds])
  useEffect(() => {
    const preferred =
      displayedSession?.anchor.kind === 'around'
        ? displayedSession.anchor.eventSequence
        : displayedSession?.anchor.kind === 'before'
          ? (timelineIds.at(-1) ?? null)
          : null
    const requested = requestedSelection.current
    if (requested !== undefined && timelineIds.includes(requested)) {
      requestedSelection.current = undefined
      dispatch(actions.timelineSelected(requested))
      rowRefs.current.get(requested)?.scrollIntoView({ block: 'nearest' })
      timelineRef.current?.focus()
      return
    }
    const reconciled = reconcileVisibleSessionSelection(
      app.selectedTimeline,
      timelineIds,
      preferred,
    )
    if (reconciled !== app.selectedTimeline) {
      dispatch(actions.timelineSelected(reconciled))
    }
  }, [app.selectedTimeline, dispatch, displayedSession?.anchor, timelineIds, timelineRef])
  useEffect(() => {
    setExpanded((current) =>
      pruneExpandedSessionItems(current, displayedSession?.window.items ?? []),
    )
  }, [displayedSession?.window.items])
  useEffect(
    () =>
      onTimelineWindowAvailable(
        timelineCapability === 'available' && displayedSession !== undefined,
      ),
    [displayedSession, onTimelineWindowAvailable, timelineCapability],
  )
  useEffect(() => () => onTimelineWindowAvailable(false), [onTimelineWindowAvailable])
  useEffect(() => {
    evictInactiveSessionWorkspaceQueries(queryClient, sessionId)
  }, [queryClient, sessionId])
  useEffect(() => {
    if (windowRequest === null || sessionId === null) return
    void loadWindow(windowRequest.anchor)
    onWindowRequestConsumed()
  }, [loadWindow, onWindowRequestConsumed, sessionId, windowRequest])
  useEffect(() => {
    if (
      awaitingSessionId === sessionId &&
      !session.isFetching &&
      (session.isSuccess || session.isError)
    ) {
      setAwaitingSessionId(null)
    }
  }, [awaitingSessionId, session.isError, session.isFetching, session.isSuccess, sessionId])
  useEffect(() => {
    if (
      sessionId !== null &&
      app.selectedTimeline !== null &&
      displayedSession?.window.items.some(
        (item) => item.address.event_sequence === app.selectedTimeline,
      )
    ) {
      dispatch(
        actions.logicalPositionRecorded({
          sessionId,
          position: app.selectedTimeline,
        }),
      )
    }
  }, [app.selectedTimeline, dispatch, displayedSession?.window.items, sessionId])
  useEffect(() => {
    if (app.selectedTimeline !== null) {
      rowRefs.current.get(app.selectedTimeline)?.scrollIntoView({ block: 'nearest' })
      eventScrollOffset.current = timelineRef.current?.scrollTop ?? 0
      restoredEventOffset.current = eventScrollOffset.current
    }
  }, [app.selectedTimeline, timelineRef])

  const selected = app.selectedTimeline
  const select = (eventSequence: string) => {
    dispatch(actions.timelineSelected(eventSequence))
  }
  const invokeTimelineCommand = (
    command:
      | 'selection.next'
      | 'selection.previous'
      | 'selection.first'
      | 'selection.last'
      | 'selection.toggleExpansion',
    control?: HTMLButtonElement,
  ) =>
    invokeCommand(command, {
      dispatch,
      getState: store.getState,
      timelineIds,
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      timelineWindowAvailable: displayedSession !== undefined,
      focusTimeline: () => timelineRef.current?.focus(),
      loadTimelineWindow: (anchor) => loadWindow(anchor, control),
      toggleTimelineExpansion: toggleSelectedExpansion,
    })
  const handleTimelineKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.target instanceof Element && event.target.closest('.session-item-detail')) return
    if ((event.key === 'Enter' || event.key === ' ') && selected !== null) {
      event.preventDefault()
      invokeTimelineCommand('selection.toggleExpansion')
      return
    }
    if (['j', 'k', 'ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
      event.currentTarget.focus()
    }
    const command = {
      ArrowDown: 'selection.next',
      ArrowUp: 'selection.previous',
      Home: 'selection.first',
      End: 'selection.last',
    }[event.key] as
      | 'selection.next'
      | 'selection.previous'
      | 'selection.first'
      | 'selection.last'
      | undefined
    if (!command) return
    event.preventDefault()
    invokeTimelineCommand(command)
  }
  const invokeBoundaryCommand = (
    command: 'selection.first' | 'selection.last',
    control: HTMLButtonElement,
  ) => {
    control.focus()
    invokeTimelineCommand(command, control)
  }

  useEffect(() => {
    if (session.error) console.error('Session load failed', session.error)
  }, [session.error])

  return (
    <section
      ref={workspaceRef}
      tabIndex={-1}
      aria-label="Session workspace"
      className="surface-body session-workspace-surface"
      onKeyDown={(event) => {
        if (
          event.key !== 'Escape' ||
          !detailsRef.current?.open ||
          !(event.target instanceof Node) ||
          !detailsRef.current.contains(event.target)
        )
          return
        const nearest =
          event.target instanceof Element ? event.target.closest('details[open]') : null
        const details = nearest instanceof HTMLDetailsElement ? nearest : detailsRef.current
        event.preventDefault()
        event.stopPropagation()
        details.open = false
        details.querySelector('summary')?.focus()
      }}
    >
      {sessionId === null || timelineCapability !== 'available' ? (
        <p className="session-entry" role="status">
          {timelineCapability === 'checking' ? (
            'Connecting…'
          ) : timelineCapability === 'unavailable' ? (
            'Session timeline unavailable'
          ) : (
            <button type="button" onClick={onReturnToCatalog}>
              Choose a session
            </button>
          )}
        </p>
      ) : session.isError ? (
        <p className="session-load-state" role="alert">
          <span>{session.isFetching ? 'Retrying session…' : 'Session failed to load.'}</span>{' '}
          <button
            type="button"
            disabled={session.isFetching}
            onClick={() => {
              workspaceRef.current?.focus()
              void refetchSession()
            }}
          >
            Retry session
          </button>
        </p>
      ) : displayedSession === undefined ? (
        <p className="session-load-state" role="status">
          Loading session…
        </p>
      ) : (
        <section className="session-workspace" aria-labelledby="session-workspace-heading">
          <p className="sr-only" role="status">
            Session {sessionId} loaded.
          </p>
          <SessionActions
            key={sessionId}
            sessionId={sessionId ?? ''}
            pendingRequest={
              live?.active?.state.kind === 'awaiting_tool_approval'
                ? live.active.state.tool_request_id
                : null
            }
            activeTurn={
              live?.active?.state.kind === 'awaiting_tool_approval'
                ? null
                : (live?.active?.turn_id ?? null)
            }
            onAccepted={refetchSession}
            renderHeader={(controls) => (
              <header className="session-compact-header">
                <div className="session-header-line">
                  <h2 id="session-workspace-heading">Session</h2>
                  <p className="session-header-state">
                    {displayedSession.descriptor.supervision?.pending
                      ? 'Recovery required'
                      : displayedSession.active
                        ? 'Active'
                        : 'Inactive'}
                  </p>
                  <span
                    className="session-header-cost"
                    data-testid="session-cost"
                    title={costLabel}
                  >
                    {costLabel}
                  </span>
                  <div
                    className="session-header-actions"
                    role="toolbar"
                    aria-label="Timeline shortcuts"
                  >
                    <button
                      type="button"
                      title="First (gg)"
                      aria-label="First"
                      onClick={(event) =>
                        invokeBoundaryCommand('selection.first', event.currentTarget)
                      }
                    >
                      <SkipBack aria-hidden="true" />
                    </button>
                    <button
                      type="button"
                      title="Latest (G)"
                      aria-label="Latest"
                      onClick={(event) =>
                        invokeBoundaryCommand('selection.last', event.currentTarget)
                      }
                    >
                      <SkipForward aria-hidden="true" />
                    </button>
                  </div>

                  <span role="status">
                    {followFailed
                      ? 'Live updates unavailable.'
                      : synchronization.phase === 'resyncing'
                        ? 'Reconnecting…'
                        : live
                          ? 'Live'
                          : 'Connecting…'}
                  </span>
                  <details ref={detailsRef} className="session-header-details">
                    <summary>Session details</summary>
                    <div className="session-header-detail-content">
                      <p>Session {sessionId}</p>
                      <p>Cost: {costLabel}</p>
                      <dl className="session-telemetry">
                        <div>
                          <dt>Items</dt>
                          <dd>{displayedSession.descriptor.sizes.item_count}</dd>
                        </div>
                        <div>
                          <dt>Active</dt>
                          <dd>{displayedSession.descriptor.work.active_turn_count}</dd>
                        </div>
                        <div>
                          <dt>Queued</dt>
                          <dd>{displayedSession.descriptor.work.queued_turn_count}</dd>
                        </div>
                        <div>
                          <dt>Up to date as of</dt>
                          <dd>{displayedSession.descriptor.observed_through}</dd>
                        </div>
                      </dl>
                      {displayedSession.descriptor.supervision && (
                        <section className="session-provenance" aria-label="Session supervision">
                          <p>
                            {displayedSession.descriptor.supervision.pending
                              ? 'Recovery required'
                              : 'Recovery recorded'}
                          </p>
                          <p>
                            {enumLabel(displayedSession.descriptor.supervision.class)} ·{' '}
                            <code>{displayedSession.descriptor.supervision.cause_code}</code>
                          </p>
                        </section>
                      )}
                      {displayedSession.descriptor.repository_watch && (
                        <section className="session-provenance" aria-label="Repository watch">
                          Repository watch ·{' '}
                          {displayedSession.descriptor.repository_watch.repository}
                          {displayedSession.descriptor.repository_watch.pull_request !== null &&
                            ` #${displayedSession.descriptor.repository_watch.pull_request}`}
                          {' · Rule '}
                          {displayedSession.descriptor.repository_watch.rule_id}
                          {' v'}
                          {displayedSession.descriptor.repository_watch.rule_revision}
                          {' · '}
                          {enumLabel(displayedSession.descriptor.repository_watch.event_kind)}
                          <details>
                            <summary>Trigger details</summary>
                            <p>
                              Trigger {displayedSession.descriptor.repository_watch.dispatch_id}
                              {' · Action '}
                              {displayedSession.descriptor.repository_watch.action_ordinal}
                            </p>
                            <p>Event {displayedSession.descriptor.repository_watch.event_id}</p>
                          </details>
                        </section>
                      )}
                      {followFailed && (
                        <button
                          type="button"
                          onClick={() => dispatch(actions.sessionFollowReconnectRequested())}
                        >
                          Reconnect live updates
                        </button>
                      )}
                      {live?.active?.state.kind === 'awaiting_credential_availability' && (
                        <p role="status" className="availability-tag">
                          Waiting for credentials · {enumLabel(live.active.state.cause)}
                        </p>
                      )}
                      {(live?.reconciliation || live?.runner) && (
                        <div className="session-live-facts">
                          {live.reconciliation && (
                            <span className="availability-tag">
                              Recovery needed · {enumLabel(live.reconciliation.kind)}
                            </span>
                          )}
                          {live.runner && (
                            <span className="availability-tag">
                              Runner: {enumLabel(live.runner.state)}
                              {live.runner.state === 'pinned' &&
                                `, ${enumLabel(live.runner.connection_health)}`}
                            </span>
                          )}
                        </div>
                      )}
                    </div>
                  </details>
                </div>
                <div className="session-header-line session-header-secondary">
                  {origin && (
                    <div className="session-header-context">
                      <a
                        href={`https://github.com/${origin.repository.split('/').map(encodeURIComponent).join('/')}`}
                      >
                        {origin.repository}
                      </a>
                      {origin.pull_request !== null && (
                        <a
                          href={`https://github.com/${origin.repository.split('/').map(encodeURIComponent).join('/')}/pull/${encodeURIComponent(origin.pull_request)}`}
                        >
                          #{origin.pull_request}
                        </a>
                      )}
                      <span title={`Rule ${origin.rule_id} · ${enumLabel(origin.event_kind)}`}>
                        Rule {origin.rule_id} · {enumLabel(origin.event_kind)}
                      </span>
                    </div>
                  )}
                  {controls}
                  <label className="session-events-toggle">
                    <input
                      type="checkbox"
                      checked={showEvents}
                      onChange={(event) => setShowEvents(event.target.checked)}
                    />
                    Events
                  </label>
                </div>
              </header>
            )}
          />
          <section
            ref={!showEvents && !transcriptAvailable ? timelineRef : undefined}
            tabIndex={!showEvents && !transcriptAvailable ? 0 : undefined}
            aria-label="Conversation"
            className="session-conversation"
          >
            {transcriptAvailable ? (
              <SessionTranscriptText
                registerUnwind={registerTranscriptUnwind}
                scrollRef={showEvents ? undefined : timelineRef}
                anchor={displayedSession.anchor}
                sessionId={sessionId ?? ''}
                first={
                  displayedSession.window.items[0]?.address.event_sequence ??
                  displayedSession.descriptor.first_address.event_sequence
                }
                through={
                  displayedSession.window.items.at(-1)?.address.event_sequence ??
                  displayedSession.descriptor.latest_address.event_sequence
                }
                observed={displayedSession.descriptor.observed_through}
                limits={transcriptLimits}
              />
            ) : (
              <p>Conversation text is unavailable; use Events to view this session.</p>
            )}
          </section>
          {synchronization.sessionId === sessionId && synchronization.drafts.length > 0 && (
            <section className="provider-drafts" aria-label="Assistant draft">
              <span>Assistant draft</span>
              {synchronization.drafts.map((draft) => (
                <p key={draft.key}>{draft.content}</p>
              ))}
            </section>
          )}
          {/* biome-ignore lint/a11y/useSemanticElements: The bounded timeline uses a scrollable ARIA grid. */}
          <div
            hidden={!showEvents}
            className={`session-timeline presentation-${app.detail}`}
            aria-label="Session timeline"
            aria-activedescendant={
              selected !== null && timelineIds.includes(selected)
                ? `session-timeline-option-${selected}`
                : undefined
            }
            ref={showEvents ? timelineRef : undefined}
            role="grid"
            tabIndex={0}
            aria-busy={session.isFetching}
            onScroll={(event) => {
              const element = event.currentTarget
              const previous = eventScrollOffset.current
              eventScrollOffset.current = element.scrollTop
              if (element.scrollTop === restoredEventOffset.current) return
              restoredEventOffset.current = null
              if (element.scrollTop !== previous)
                traverseEventEdge(element, element.scrollTop < previous ? 'before' : 'after')
            }}
            onWheel={(event) => {
              if (event.deltaY !== 0)
                traverseEventEdge(event.currentTarget, event.deltaY < 0 ? 'before' : 'after')
            }}
            onTouchStart={(event) => {
              eventTouchPosition.current = event.touches[0]?.clientY ?? null
            }}
            onTouchMove={(event) => {
              const position = event.touches[0]?.clientY
              const previous = eventTouchPosition.current
              eventTouchPosition.current = position ?? null
              if (position !== undefined && previous !== null && position !== previous)
                traverseEventEdge(event.currentTarget, position > previous ? 'before' : 'after')
            }}
            onKeyDown={(event) => {
              if (event.target === event.currentTarget) {
                if (event.key === 'PageUp') traverseEventEdge(event.currentTarget, 'before')
                if (event.key === 'PageDown') traverseEventEdge(event.currentTarget, 'after')
              }
              handleTimelineKeyDown(event)
            }}
          >
            {items.map((item) => {
              const id = item.address.event_sequence
              const isExpanded = expanded.has(id)
              return (
                // biome-ignore lint/a11y/useSemanticElements: Expanded timeline rows use the scrollable grid layout.
                <div
                  id={`session-timeline-option-${id}`}
                  key={id}
                  role="row"
                  aria-controls={`session-timeline-detail-${id}`}
                  aria-describedby={`session-timeline-disclosure-${id}`}
                  aria-selected={selected === id}
                  aria-expanded={isExpanded}
                  tabIndex={-1}
                  ref={(node) => {
                    if (node) rowRefs.current.set(id, node)
                    else rowRefs.current.delete(id)
                  }}
                  className={selected === id ? 'selected' : undefined}
                  onClick={(event) => {
                    if (
                      event.target instanceof Element &&
                      event.target.closest('.session-item-detail')
                    )
                      return
                    select(id)
                    invokeTimelineCommand('selection.toggleExpansion')
                    timelineRef.current?.focus()
                  }}
                  onKeyDown={(event) => {
                    if (event.target !== event.currentTarget) return
                    if (event.key !== 'Enter' && event.key !== ' ') return
                    event.preventDefault()
                    event.stopPropagation()
                    select(id)
                    invokeTimelineCommand('selection.toggleExpansion')
                    timelineRef.current?.focus()
                  }}
                >
                  {/* biome-ignore lint/a11y/useSemanticElements: Detail content uses the scrollable grid layout. */}
                  <div role="gridcell" tabIndex={-1}>
                    <span id={`session-timeline-disclosure-${id}`} className="sr-only">
                      {isExpanded ? 'Expanded' : 'Collapsed'}
                    </span>
                    <div className="session-item-summary">
                      {isExpanded ? (
                        <ChevronDown aria-hidden="true" />
                      ) : (
                        <ChevronRight aria-hidden="true" />
                      )}
                      <span className="session-address">{id}</span>
                      <strong>{enumLabel(item.kind)}</strong>
                      <small>{item.projected_structured_bytes} B</small>
                    </div>
                    {isExpanded && sessionId !== null && (
                      <div id={`session-timeline-detail-${id}`} className="session-item-detail">
                        {transcriptAvailable ? (
                          <SessionItemDetail
                            key={`${sessionId}:${id}`}
                            sessionId={sessionId}
                            item={item}
                            limits={transcriptLimits}
                            onComplete={() => timelineRef.current?.focus()}
                          />
                        ) : (
                          <p>Timeline detail unavailable.</p>
                        )}
                      </div>
                    )}
                  </div>
                </div>
              )
            })}
          </div>
        </section>
      )}
      {sessionId !== null && timelineCapability === 'available' && (
        <SessionComposer
          key={sessionId}
          sessionId={sessionId ?? ''}
          activeState={live ? (live.active?.state.kind ?? null) : undefined}
          stateUnavailable={followFailed && live === null}
          supervision={displayedSession?.descriptor.supervision ?? null}
          onAccepted={refetchSession}
          onEscape={() => (timelineRef.current ?? workspaceRef.current)?.focus()}
        />
      )}
    </section>
  )
}
