import { useQuery } from '@tanstack/react-query'
import { useVirtualizer } from '@tanstack/react-virtual'
import { useEffect, useRef } from 'react'
import type { WebTemplateSummary } from '../../generated/web-contract.mjs'
import { HttpTemplateApi, type TemplateApi } from './api'
import { grantLabel, operationLabel, postureLabels } from './presentation'
import './templates.css'

const httpApi = new HttpTemplateApi()

function Grants({ template }: { template: WebTemplateSummary }) {
  return (
    <>
      <p>
        {template.dangerous_tool_auto_approval
          ? 'Automatic approval for dangerous tools is on'
          : 'Automatic approval for dangerous tools is off'}
      </p>
      {template.workflow_tools.length === 0 ? (
        <p>No workflow permissions</p>
      ) : (
        <dl className="template-grants">
          {template.workflow_tools.map((tool) => (
            <div key={tool.name}>
              <dt>{operationLabel(tool.name)}</dt>
              <dd>
                {grantLabel(tool)}
                {' · '}
                {postureLabels[tool.posture]}
              </dd>
            </div>
          ))}
        </dl>
      )}
    </>
  )
}

export function TemplatesSurface({
  api = httpApi,
  selectedName,
  onSelect,
}: {
  api?: TemplateApi
  selectedName?: string
  onSelect: (name?: string) => void
}) {
  const catalog = useQuery({
    queryKey: ['templates', api],
    queryFn: ({ signal }) => api.list(signal),
  })
  const detail = useQuery({
    queryKey: ['template', api, selectedName],
    queryFn: ({ signal }) => api.detail(selectedName ?? '', signal),
    enabled: selectedName !== undefined,
    gcTime: 0,
  })
  const scroll = useRef<HTMLDivElement>(null)
  const returnName = useRef<string | undefined>(undefined)
  const restoreFocus = useRef(false)
  const templates = catalog.data?.templates ?? []
  const rows = useVirtualizer({
    count: templates.length,
    getScrollElement: () => scroll.current,
    estimateSize: () => 64,
    overscan: 8,
  })

  useEffect(() => {
    if (selectedName !== undefined) {
      returnName.current = selectedName
      restoreFocus.current = true
    } else if (restoreFocus.current) {
      const index = templates.findIndex((template) => template.name === returnName.current)
      if (index >= 0) rows.scrollToIndex(index)
    }
  }, [selectedName, templates, rows])

  if (selectedName !== undefined)
    return (
      <section className="templates-surface" aria-label="Template definition">
        <button type="button" className="button subtle" onClick={() => onSelect()}>
          Back to templates
        </button>
        {detail.isPending && <p role="status">Loading template…</p>}
        {detail.error && <p role="alert">{detail.error.message}</p>}
        {detail.data && (
          <>
            <div className="template-heading">
              <div>
                <h2>{detail.data.summary.name}</h2>
                <p>{detail.data.summary.model_label}</p>
              </div>
              <div>
                <button
                  type="button"
                  className="button primary"
                  disabled
                  aria-describedby="template-start-reason"
                >
                  Start session
                </button>
                <p id="template-start-reason" className="template-muted">
                  Starting sessions is not available yet.
                </p>
              </div>
            </div>
            <Grants template={detail.data.summary} />
            <details>
              <summary>System instructions</summary>
              <pre className="template-prompt">{detail.data.system_prompt}</pre>
            </details>
            <details>
              <summary>Definition and digest</summary>
              <p className="template-digest">{detail.data.summary.digest}</p>
              {detail.data.source_kind === 'review_library' && (
                <p>This definition is shared by the review templates.</p>
              )}
              <pre className="template-source">{detail.data.definition_toml}</pre>
            </details>
          </>
        )}
      </section>
    )

  return (
    <section className="templates-surface" aria-label="Templates">
      <p className="template-muted">Choose a starting point for a session.</p>
      {catalog.isPending && <p role="status">Loading templates…</p>}
      {catalog.error && <p role="alert">{catalog.error.message}</p>}
      {catalog.data && templates.length === 0 && <p>No templates are configured.</p>}
      <div ref={scroll} className="template-list">
        <div style={{ height: rows.getTotalSize(), position: 'relative' }}>
          {rows.getVirtualItems().map((row) => {
            const template = templates[row.index]
            if (!template) return null
            return (
              <button
                type="button"
                key={template.name}
                className="template-row"
                ref={(node) => {
                  if (node && restoreFocus.current && returnName.current === template.name) {
                    node.focus()
                    restoreFocus.current = false
                  }
                }}
                onClick={() => onSelect(template.name)}
                style={{
                  position: 'absolute',
                  top: 0,
                  left: 0,
                  width: '100%',
                  height: row.size,
                  transform: `translateY(${row.start}px)`,
                }}
              >
                <span className="template-row-name">
                  <span>{template.name}</span>
                  <span className="template-muted">{template.model_label}</span>
                  <span className="template-muted" title={template.digest}>
                    Digest {template.digest.slice(0, 12)}
                  </span>
                </span>
                <span className="template-row-summary">
                  <span
                    title={template.workflow_tools
                      .map(
                        (tool) =>
                          `${operationLabel(tool.name)}: ${grantLabel(tool)} · ${postureLabels[tool.posture]}`,
                      )
                      .join('; ')}
                  >
                    Workflow permissions: {template.workflow_tools.length}
                  </span>
                  <span className="template-muted">
                    {template.dangerous_tool_auto_approval
                      ? 'Dangerous tools: auto-approval on'
                      : 'Dangerous tools: auto-approval off'}
                  </span>
                </span>
              </button>
            )
          })}
        </div>
      </div>
    </section>
  )
}
