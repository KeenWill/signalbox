import { useMutation, useQueryClient } from '@tanstack/react-query'
import { useId, useState } from 'react'
import type { WebTemplateDetail } from '../../generated/web-contract.mjs'
import type { TemplateApi } from './api'

export function TemplateEditor({ api, detail }: { api: TemplateApi; detail: WebTemplateDetail }) {
  const id = useId()
  const [source, setSource] = useState(detail.definition_toml)
  const queries = useQueryClient()
  const save = useMutation({
    mutationFn: (definition: string) =>
      api.save(detail.summary.name, { definition_toml: definition }),
    onSuccess: (saved) => {
      setSource(saved.definition_toml)
      queries.setQueryData(['template', api, saved.summary.name], saved)
      void queries.invalidateQueries({ queryKey: ['templates', api] })
      void queries.invalidateQueries({ queryKey: ['template', api] })
    },
  })
  return (
    <details className="template-editor">
      <summary>Edit template</summary>
      {detail.source_kind === 'review_library' && (
        <p>Saving this shared definition updates the review templates it generates.</p>
      )}
      <form
        onSubmit={(event) => {
          event.preventDefault()
          save.mutate(source)
        }}
      >
        <label htmlFor={id}>Template definition (TOML)</label>
        <textarea
          id={id}
          value={source}
          spellCheck={false}
          disabled={save.isPending}
          aria-invalid={save.isError}
          aria-describedby={save.isError ? `${id}-error` : undefined}
          onChange={(event) => {
            setSource(event.target.value)
            save.reset()
          }}
        />
        {save.error && (
          <p id={`${id}-error`} role="alert">
            {save.error.message}
          </p>
        )}
        <div className="template-editor-actions">
          <button type="submit" className="button primary" disabled={save.isPending}>
            {save.isPending ? 'Saving…' : 'Save template'}
          </button>
          {save.isSuccess && <p role="status">Template saved.</p>}
        </div>
      </form>
    </details>
  )
}
