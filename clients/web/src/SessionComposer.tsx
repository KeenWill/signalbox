import { useMutation } from '@tanstack/react-query'
import { ArrowUp, Paperclip } from 'lucide-react'
import { useEffect, useState } from 'react'
import { invokeCommand } from './commands'
import type {
  WebSessionTimelineDescriptor,
  WebSubmitInputRequest,
} from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import {
  MAX_SESSION_MESSAGE_LENGTH,
  ProductInputError,
  ProductRequestError,
  submitSessionInput,
} from './product'
import {
  actions,
  selectPendingSessionInput,
  selectSessionInputCapacityReached,
  store,
  useAppDispatch,
  useAppSelector,
} from './state'

export function SessionComposer({
  sessionId,
  activeState,
  stateUnavailable,
  supervision,
  onAccepted,
  onEscape,
}: {
  sessionId: string
  activeState: string | null | undefined
  stateUnavailable: boolean
  supervision: WebSessionTimelineDescriptor['supervision']
  onAccepted: () => Promise<unknown>
  onEscape: () => void
}) {
  const dispatch = useAppDispatch()
  const pending = useAppSelector((state) => selectPendingSessionInput(state, sessionId))
  const retained = pending?.input ?? null
  const capacityReached = useAppSelector(selectSessionInputCapacityReached)
  const capacityNotice = 'Too many unconfirmed messages'
  const newInputBlocked = retained === null && capacityReached
  const [text, setText] = useState('')
  const [notice, setNotice] = useState('')
  useEffect(() => {
    if (notice === 'Message accepted' && activeState != null) setNotice('')
  }, [activeState, notice])
  const mutation = useMutation({
    mutationFn: (input: WebSubmitInputRequest) => submitSessionInput(sessionId, input),
    onSuccess: (_, input) => {
      dispatch(
        actions.sessionInputSettled({ sessionId, commandId: input.command_id, confirmed: true }),
      )
      setText('')
      setNotice('Message accepted')
      void onAccepted()
    },
    onError: (error, input) => {
      if (
        error instanceof ProductInputError ||
        (error instanceof ProductRequestError && error.status < 500)
      ) {
        dispatch(
          actions.sessionInputSettled({ sessionId, commandId: input.command_id, confirmed: true }),
        )
        setText(input.message)
        setNotice(`Message rejected: ${error.message}`)
      } else {
        dispatch(
          actions.sessionInputSettled({ sessionId, commandId: input.command_id, confirmed: false }),
        )
      }
    },
  })
  const canSend =
    !newInputBlocked &&
    pending?.phase !== 'sending' &&
    (retained !== null || (activeState === null && text.length > 0))
  const send = () => {
    if (!canSend) return
    const current = selectPendingSessionInput(store.getState(), sessionId)
    if (current?.phase === 'sending') return
    const input = current?.input ?? { command_id: crypto.randomUUID(), message: text }
    dispatch(actions.sessionInputStarted({ sessionId, input }))
    if (
      selectPendingSessionInput(store.getState(), sessionId)?.input.command_id !== input.command_id
    ) {
      return
    }
    setNotice('Sending…')
    mutation.mutate(input)
  }
  const invokeSend = () =>
    invokeCommand('session.send', {
      dispatch,
      getState: store.getState,
      timelineIds: [],
      artifactPreviewIds: [],
      artifactOriginalIds: [],
      focusTimeline: () => undefined,
      submitSessionInput: canSend ? send : undefined,
    })
  const disabledReason = newInputBlocked
    ? capacityNotice
    : pending?.phase === 'sending'
      ? 'Sending…'
      : retained !== null
        ? 'Delivery unconfirmed'
        : stateUnavailable
          ? 'Session unavailable'
          : activeState === undefined
            ? 'Connecting…'
            : activeState !== null
              ? `Wait for the current turn to finish · ${enumLabel(activeState)}`
              : text.length === 0
                ? 'Write a message to send'
                : ''
  return (
    <form
      className="session-composer"
      aria-label="Message composer"
      onSubmit={(event) => {
        event.preventDefault()
        invokeSend()
      }}
    >
      <label className="sr-only" htmlFor="session-message">
        Message
      </label>
      <textarea
        id="session-message"
        rows={2}
        aria-describedby="session-composer-status session-composer-help"
        placeholder="Write a message…"
        maxLength={MAX_SESSION_MESSAGE_LENGTH}
        value={retained?.message ?? text}
        readOnly={retained !== null}
        onKeyDown={(event) => {
          if (
            event.key === 'Enter' &&
            !event.shiftKey &&
            !event.altKey &&
            !event.ctrlKey &&
            !event.metaKey &&
            !event.nativeEvent.isComposing &&
            event.keyCode !== 229
          ) {
            event.preventDefault()
            event.stopPropagation()
            invokeSend()
            return
          }
          if (event.key !== 'Escape') return
          event.preventDefault()
          event.stopPropagation()
          onEscape()
        }}
        onChange={(event) => {
          if (event.target.value.length <= MAX_SESSION_MESSAGE_LENGTH) setText(event.target.value)
        }}
      />
      <div className="session-composer-actions">
        <span className="session-attachment-affordance" title="File attachments are unavailable">
          <button
            type="button"
            disabled
            aria-label="Attach files (unavailable)"
            aria-describedby="session-attachment-help"
          >
            <Paperclip aria-hidden="true" />
          </button>
          <span id="session-attachment-help">Attachments unavailable</span>
        </span>
        <span id="session-composer-help">Enter to send · Shift+Enter for newline</span>
        <button type="submit" disabled={!canSend} aria-describedby="session-composer-status">
          <ArrowUp aria-hidden="true" />
          {retained === null
            ? 'Send message'
            : pending?.phase === 'sending'
              ? 'Sending…'
              : 'Retry message'}
        </button>
        <span id="session-composer-status" role="status">
          {supervision?.pending
            ? 'Session recovery required'
            : notice.startsWith('Message rejected:')
              ? [notice, disabledReason].filter(Boolean).join(' · ')
              : activeState === null && retained === null && !newInputBlocked
                ? notice || disabledReason
                : disabledReason || notice}
        </span>
      </div>
    </form>
  )
}
