import { useMutation, useQueryClient } from '@tanstack/react-query'
import { useEffect, useId, useRef, useState } from 'react'
import type { WebTemplateDetail } from '../../generated/web-contract.mjs'
import type { TemplateApi } from './api'

export function TemplateEditor({ api, detail }: { api: TemplateApi; detail: WebTemplateDetail }) {
  const id = useId()
  const editor = useRef<HTMLTextAreaElement>(null)
  const [draft, setDraft] = useState<{ source: string; original: string } | null>(null)
  const source = draft?.source ?? detail.definition_toml
  const conflict =
    draft !== null &&
    draft.source !== detail.definition_toml &&
    draft.original !== detail.definition_toml
  const queries = useQueryClient()
  const save = useMutation({
    mutationFn: (definition: string) =>
      api.save(detail.summary.name, { definition_toml: definition }),
    onSuccess: (saved) => {
      setDraft(null)
      queries.setQueryData(['template', api, saved.summary.name], saved)
      void queries.invalidateQueries({ queryKey: ['templates', api] })
      void queries.invalidateQueries({ queryKey: ['template', api] })
    },
  })
  useEffect(() => {
    if (draft?.source === detail.definition_toml && !save.isPending) {
      setDraft(null)
      save.reset()
    }
  }, [draft, detail.definition_toml, save.isPending, save.reset])
  return (
    <details className="template-editor">
      <summary>Edit template</summary>
      {detail.source_kind === 'review_library' && (
        <p>Saving this shared definition updates the review templates it generates.</p>
      )}
      <form
        onSubmit={(event) => {
          event.preventDefault()
          if (conflict) return
          save.mutate(source)
        }}
      >
        <label htmlFor={id}>Template definition (TOML)</label>
        <textarea
          id={id}
          ref={editor}
          value={source}
          spellCheck={false}
          disabled={save.isPending}
          aria-invalid={save.isError}
          aria-describedby={conflict ? `${id}-conflict` : save.isError ? `${id}-error` : undefined}
          onChange={(event) => {
            const source = event.target.value
            setDraft((previous) =>
              source === detail.definition_toml
                ? null
                : { source, original: previous?.original ?? detail.definition_toml },
            )
            save.reset()
          }}
        />
        {conflict && (
          <p id={`${id}-conflict`} role="alert">
            This template changed while you were editing. Copy any edits you want to keep, then use
            the latest definition before saving.
          </p>
        )}
        {save.error && (
          <p id={`${id}-error`} role="alert">
            {save.error.message}
          </p>
        )}
        <div className="template-editor-actions">
          <button type="submit" className="button primary" disabled={save.isPending || conflict}>
            {save.isPending ? 'Saving…' : 'Save template'}
          </button>
          {conflict && (
            <button
              type="button"
              className="button"
              disabled={save.isPending}
              onClick={() => {
                setDraft(null)
                save.reset()
                editor.current?.focus()
              }}
            >
              Discard edits and use latest
            </button>
          )}
          {save.isSuccess && <p role="status">Template saved.</p>}
        </div>
      </form>
    </details>
  )
}
