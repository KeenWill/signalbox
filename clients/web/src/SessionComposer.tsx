import { useMutation } from '@tanstack/react-query'
import { useState } from 'react'
import { invokeCommand } from './commands'
import type { WebSubmitInputRequest } from './generated/web-contract.mjs'
import { ProductInputError, ProductRequestError, submitSessionInput } from './product'
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
  onAccepted,
}: {
  sessionId: string
  activeState: string | null | undefined
  stateUnavailable: boolean
  onAccepted: () => Promise<unknown>
}) {
  const dispatch = useAppDispatch()
  const pending = useAppSelector((state) => selectPendingSessionInput(state, sessionId))
  const retained = pending?.input ?? null
  const capacityReached = useAppSelector(selectSessionInputCapacityReached)
  const capacityNotice =
    'Pending-message limit reached. Retry a retained message before sending to another session.'
  const newInputBlocked = retained === null && capacityReached
  const [text, setText] = useState('')
  const [notice, setNotice] = useState('')
  const mutation = useMutation({
    mutationFn: (input: WebSubmitInputRequest) => submitSessionInput(sessionId, input),
    onSuccess: (_, input) => {
      dispatch(
        actions.sessionInputSettled({ sessionId, commandId: input.command_id, confirmed: true }),
      )
      setText('')
      setNotice('Message accepted by the daemon.')
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
        setNotice(`Rejected: ${error.message}`)
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
    setNotice('Sending message…')
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
  return (
    <form
      className="session-composer"
      aria-label="Message composer"
      onSubmit={(event) => {
        event.preventDefault()
        invokeSend()
      }}
    >
      <header>
        <h3>Message</h3>
        <span>
          {stateUnavailable
            ? 'Input state unavailable'
            : activeState === undefined
              ? 'Checking input state…'
              : activeState === null
                ? 'Starts a turn when idle'
                : `Input unavailable: ${activeState.replaceAll('_', ' ')}`}
        </span>
      </header>
      <label htmlFor="session-message">Message to session</label>
      <textarea
        id="session-message"
        rows={3}
        value={retained?.message ?? text}
        readOnly={retained !== null}
        onChange={(event) => setText(event.target.value)}
      />
      <div className="session-composer-actions">
        <button type="submit" disabled={!canSend}>
          {retained === null
            ? 'Send message'
            : pending?.phase === 'sending'
              ? 'Sending…'
              : 'Retry message'}
        </button>
        <span role="status">
          {newInputBlocked
            ? capacityNotice
            : pending?.phase === 'unconfirmed'
              ? 'Acceptance is unconfirmed. Retry sends the same command and message.'
              : pending?.phase === 'sending'
                ? 'Sending message…'
                : notice}
        </span>
      </div>
    </form>
  )
}
