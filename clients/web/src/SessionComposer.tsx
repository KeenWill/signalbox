import { useMutation } from '@tanstack/react-query'
import { useEffect, useState } from 'react'
import { invokeCommand } from './commands'
import type { WebSessionLiveSnapshot, WebSubmitInputRequest } from './generated/web-contract.mjs'
import {
  followSession,
  ProductInputError,
  ProductRequestError,
  readSessionLive,
  submitSessionInput,
} from './product'
import { store, useAppDispatch } from './state'

export function useSessionFollow(sessionId: string | null, onDurable: () => Promise<unknown>) {
  const [live, setLive] = useState<WebSessionLiveSnapshot | null>(null)
  const [error, setError] = useState(false)
  const [attempt, setAttempt] = useState(0)
  // biome-ignore lint/correctness/useExhaustiveDependencies: reconnect explicitly opens a new stream.
  useEffect(() => {
    setLive(null)
    setError(false)
    if (sessionId === null) return
    const controller = new AbortController()
    void (async () => {
      try {
        for await (const event of followSession(sessionId, controller.signal)) {
          if (event.kind === 'snapshot') {
            setLive(event.snapshot)
            await onDurable()
          }
          if (event.kind === 'durable') {
            await onDurable()
            const snapshot = await readSessionLive(sessionId, controller.signal)
            setLive(snapshot)
          }
        }
      } catch {
        if (!controller.signal.aborted) setError(true)
      }
    })()
    return () => controller.abort()
  }, [sessionId, onDurable, attempt])
  return {
    live: live?.session_id === sessionId ? live : null,
    error,
    reconnect: () => setAttempt((value) => value + 1),
  }
}

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
  const [text, setText] = useState('')
  const [retained, setRetained] = useState<WebSubmitInputRequest | null>(null)
  const [notice, setNotice] = useState('')
  const mutation = useMutation({
    mutationFn: (input: WebSubmitInputRequest) => submitSessionInput(sessionId, input),
    onSuccess: () => {
      setRetained(null)
      setText('')
      setNotice('Message accepted by the daemon.')
      void onAccepted()
    },
    onError: (error) => {
      if (
        error instanceof ProductInputError ||
        (error instanceof ProductRequestError && error.status < 500)
      ) {
        setRetained(null)
        setNotice(`Rejected: ${error.message}`)
      } else {
        setNotice('Acceptance is unconfirmed. Retry sends the same command and message.')
      }
    },
  })
  const canSend =
    !mutation.isPending && (retained !== null || (activeState === null && text.length > 0))
  const send = () => {
    if (!canSend) return
    const input = retained ?? { command_id: crypto.randomUUID(), message: text }
    setRetained(input)
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
        value={text}
        readOnly={retained !== null || mutation.isPending}
        onChange={(event) => setText(event.target.value)}
      />
      <div className="session-composer-actions">
        <button type="submit" disabled={!canSend}>
          {retained === null ? 'Send message' : mutation.isPending ? 'Sending…' : 'Retry message'}
        </button>
        <span role="status">{notice}</span>
      </div>
    </form>
  )
}
