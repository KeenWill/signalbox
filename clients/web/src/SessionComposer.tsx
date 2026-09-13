import { useMutation } from '@tanstack/react-query'
import { ArrowUp } from 'lucide-react'
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { invokeCommand } from './commands'
import { Field } from './Field'
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
  const messageRef = useRef<HTMLTextAreaElement>(null)
  const message = retained?.message ?? text
  // biome-ignore lint/correctness/useExhaustiveDependencies: Measure after the rendered message changes.
  useLayoutEffect(() => {
    const field = messageRef.current
    if (!field) return
    const resize = () => {
      field.style.height = 'auto'
      field.style.height = `${field.scrollHeight}px`
    }
    resize()
    let width = 0
    let frame = 0
    const observer = new ResizeObserver(([entry]) => {
      if (!entry || width === entry.contentRect.width) return
      width = entry.contentRect.width
      cancelAnimationFrame(frame)
      frame = requestAnimationFrame(resize)
    })
    observer.observe(field)
    return () => {
      observer.disconnect()
      cancelAnimationFrame(frame)
    }
  }, [message])
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
      <Field
        as="textarea"
        label="Message"
        ref={messageRef}
        id="session-message"
        rows={1}
        aria-describedby="session-composer-status session-composer-help"
        placeholder="Write a message…"
        maxLength={MAX_SESSION_MESSAGE_LENGTH}
        value={message}
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
            if (!event.repeat) invokeSend()
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
        <span id="session-composer-help" className="sr-only">
          Enter to send · Shift+Enter for newline
        </span>
        <button
          type="submit"
          disabled={!canSend}
          aria-label={
            retained === null
              ? 'Send message'
              : pending?.phase === 'sending'
                ? 'Sending…'
                : 'Retry message'
          }
          aria-describedby="session-composer-status"
          title="Enter to send · Shift+Enter for newline"
        >
          <ArrowUp aria-hidden="true" />
          {retained === null ? 'Send' : pending?.phase === 'sending' ? 'Sending…' : 'Retry'}
        </button>
        <span
          id="session-composer-status"
          role="status"
          className={
            activeState === null &&
            retained === null &&
            !newInputBlocked &&
            !notice &&
            !supervision?.pending
              ? 'sr-only'
              : undefined
          }
        >
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
