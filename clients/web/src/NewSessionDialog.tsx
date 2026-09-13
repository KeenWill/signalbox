import * as Dialog from '@radix-ui/react-dialog'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { X } from 'lucide-react'
import { type RefObject, useEffect, useRef, useState } from 'react'
import { HttpTemplateApi } from './features/templates/api'
import type { WebCreateSessionRequest } from './generated/web-contract.mjs'
import {
  clearRetainedCreation,
  HttpSessionCreationApi,
  readRetainedCreation,
  retainCreation,
  SessionCreationRejected,
} from './newSession'
import { invokeProductCommand, type ProductCommandContext } from './productCommands'
import { useAppSelector } from './state'
import './new-session.css'

const templatesApi = new HttpTemplateApi()
const creationApi = new HttpSessionCreationApi()

export function NewSessionDialog({
  context,
  openerRef,
  fallbackRef,
}: {
  context: ProductCommandContext
  openerRef: RefObject<HTMLElement | null>
  fallbackRef: RefObject<HTMLElement | null>
}) {
  const open = useAppSelector((state) => state.app.overlay === 'new-session')
  const queryClient = useQueryClient()
  const [template, setTemplate] = useState('')
  const [retained, setRetained] = useState<WebCreateSessionRequest | null>(null)
  const [storageError, setStorageError] = useState<string | null>(null)
  const submitted = useRef(false)
  useEffect(() => {
    if (!open) return
    try {
      setRetained(readRetainedCreation())
      setStorageError(null)
    } catch {
      setStorageError('The saved session request could not be read.')
    }
  }, [open])
  const templates = useQuery({
    queryKey: ['production', 'templates'],
    queryFn: ({ signal }) => templatesApi.list(signal),
    enabled: open && retained === null,
  })
  const selected =
    retained?.template_name ??
    (templates.data?.templates.some((item) => item.name === template)
      ? template
      : (templates.data?.templates[0]?.name ?? ''))
  const creation = useMutation({
    retry: false,
    mutationFn: async () => {
      const request = readRetainedCreation() ?? {
        command_id: crypto.randomUUID(),
        template_name: selected,
        first_input: null,
      }
      retainCreation(request)
      setRetained(request)
      return creationApi.create(request)
    },
    onSuccess: (result) => {
      clearRetainedCreation()
      setRetained(null)
      submitted.current = true
      void queryClient.invalidateQueries({ queryKey: ['production', 'sessions'] })
      invokeProductCommand('surface.escape', context)
      invokeProductCommand('session.open', { ...context, sessionId: result.session_id })
    },
    onError: (error) => {
      if (error instanceof SessionCreationRejected) {
        clearRetainedCreation()
        setRetained(null)
      }
    },
  })
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!next && !creation.isPending) invokeProductCommand('surface.escape', context)
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="dialog-overlay" />
        <Dialog.Content
          className="dialog-content new-session-dialog"
          onEscapeKeyDown={(event) => {
            event.stopPropagation()
            if (creation.isPending) event.preventDefault()
          }}
          onOpenAutoFocus={() => {
            submitted.current = false
          }}
          onCloseAutoFocus={(event) => {
            event.preventDefault()
            const opener = openerRef.current
            if (!submitted.current && opener?.isConnected && opener.getClientRects().length > 0)
              opener.focus()
            else fallbackRef.current?.focus()
          }}
        >
          <div className="dialog-heading">
            <Dialog.Title>New session</Dialog.Title>
            <Dialog.Close asChild>
              <button
                type="button"
                className="icon-button"
                aria-label="Close new session"
                disabled={creation.isPending}
              >
                <X />
              </button>
            </Dialog.Close>
          </div>
          <Dialog.Description>Choose a template to start a conversation.</Dialog.Description>
          <form
            className="new-session-form"
            onSubmit={(event) => {
              event.preventDefault()
              if (!creation.isPending && selected && !storageError && !context.navigationLocked)
                creation.mutate()
            }}
          >
            {retained ? (
              <p>Template: {retained.template_name}</p>
            ) : (
              <>
                <label htmlFor="new-session-template">Template</label>
                <select
                  id="new-session-template"
                  value={selected}
                  onChange={(event) => setTemplate(event.currentTarget.value)}
                  disabled={creation.isPending || !templates.data?.templates.length}
                >
                  {!templates.data?.templates.length && (
                    <option value="">
                      {templates.isPending ? 'Loading templates…' : 'No templates available'}
                    </option>
                  )}
                  {templates.data?.templates.map((item) => (
                    <option key={item.name} value={item.name}>
                      {item.name} — {item.model_label}
                    </option>
                  ))}
                </select>
                {templates.isError && (
                  <div role="alert">
                    Could not load templates.{' '}
                    <button
                      type="button"
                      onClick={() => {
                        void templates.refetch()
                      }}
                    >
                      Retry templates
                    </button>
                  </div>
                )}
              </>
            )}
            {storageError && <p role="alert">{storageError}</p>}
            {creation.isError && <p role="alert">{creation.error.message}</p>}
            {retained && !creation.isPending && <p>Retry to finish creating this session.</p>}
            <button
              type="submit"
              disabled={
                creation.isPending || !selected || storageError !== null || context.navigationLocked
              }
            >
              {creation.isPending ? 'Creating…' : retained ? 'Retry creation' : 'Create session'}
            </button>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  )
}
