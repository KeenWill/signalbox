import { type ComponentType, useState } from 'react'
import type {
  WebSessionTimelineDetailBody,
  WebTimelineTextExcerpt,
  WebTimelineToolAttempt,
} from '../../generated/web-contract.mjs'
import { enumLabel } from '../../labels'
import { ARTIFACT_PREVIEW_CHARACTERS, ARTIFACT_PREVIEW_LINES } from '../artifacts/artifactTypes'
import { ToolResultMedia } from './ToolResultMedia'
import {
  excerptFields,
  type Fields,
  fieldLabel,
  fields,
  previewText,
  searchResultText,
  textField,
  webLink,
} from './toolPresentation'
import './tools.css'

export interface ToolCallProps {
  tool: WebTimelineToolAttempt
  showMedia?: boolean
}

interface RendererProps {
  arguments: Fields
  result: Fields
  resultExcerpt?: WebTimelineTextExcerpt | null
}

function TextPreview({
  text,
  label,
  diff = false,
  expandable = false,
}: {
  text: string
  label: string
  diff?: boolean
  expandable?: boolean
}) {
  const [expanded, setExpanded] = useState(false)
  const preview = previewText(text)
  const content = expanded ? text : preview.content
  if (!text) return null
  return (
    <section aria-label={label}>
      <pre className={diff ? 'tool-diff' : undefined}>
        <code>
          {diff
            ? content.split(/(?<=\n)/u).map((line, index) => (
                <span
                  // biome-ignore lint/suspicious/noArrayIndexKey: Immutable lines can repeat.
                  key={index}
                  data-change={
                    line.startsWith('+') ? 'added' : line.startsWith('-') ? 'removed' : undefined
                  }
                >
                  {line}
                </span>
              ))
            : content}
        </code>
      </pre>
      {preview.omittedCharacters > 0 &&
        (expandable ? (
          <button type="button" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
            {expanded ? 'Show less' : 'Show all fetched text'}
          </button>
        ) : (
          <small>Showing part of the text</small>
        ))}
    </section>
  )
}

function Excerpt({
  excerpt,
  label,
  expandable = false,
}: {
  excerpt?: WebTimelineTextExcerpt | null
  label: string
  expandable?: boolean
}) {
  if (!excerpt) return null
  return (
    <>
      {expandable ? (
        <TextPreview text={excerpt.text} label={label} expandable />
      ) : (
        <PresentedExcerpt excerpt={excerpt} label={label} />
      )}
      {(excerpt.offset_bytes !== '0' || excerpt.continuation != null) && (
        <small>Showing part of {label.toLowerCase()}</small>
      )}
    </>
  )
}

function valueSummary(value: unknown): string {
  if (value === null) return 'None'
  if (Array.isArray(value)) {
    const entries = value
      .slice(0, ARTIFACT_PREVIEW_LINES)
      .map((item) =>
        item === null
          ? 'None'
          : Array.isArray(item)
            ? `List · ${item.length} items`
            : typeof item === 'object'
              ? `Object · ${Object.keys(item).length} fields`
              : previewText(textField(item)).content,
      )
    return `${value.length} items${entries.length ? `: ${entries.join(', ')}` : ''}${value.length > entries.length ? ' · More in Raw' : ''}`
  }
  if (typeof value === 'object') return `Object · ${Object.keys(value).length} fields`
  return textField(value)
}

function FieldList({ value }: { value: Fields }) {
  const entries = Object.entries(value)
  let remaining = ARTIFACT_PREVIEW_CHARACTERS
  const rows: { key: string; label: string; text: string }[] = []
  let trimmed = false
  for (const [key, item] of entries) {
    if (rows.length === ARTIFACT_PREVIEW_LINES || remaining === 0) {
      trimmed = true
      break
    }
    const label = Array.from(fieldLabel(key)).slice(0, remaining).join('')
    remaining -= Array.from(label).length
    const summary = valueSummary(item)
    const preview = previewText(summary)
    const text = Array.from(preview.content).slice(0, remaining).join('')
    remaining -= Array.from(text).length
    trimmed ||= label !== fieldLabel(key) || text !== summary
    rows.push({ key, label, text: summary === '' ? 'Empty text' : text })
  }
  return (
    <>
      {entries.length > 0 && (
        <dl className="tool-fields">
          {rows.map(({ key, label, text }) => (
            <div key={key}>
              <dt>{label}</dt>
              <dd>{text}</dd>
            </div>
          ))}
        </dl>
      )}
      {trimmed && <small>Showing part of the details · Open Raw for all fetched text</small>}
    </>
  )
}

function PresentedExcerpt({ excerpt, label }: { excerpt: WebTimelineTextExcerpt; label: string }) {
  const value = excerptFields(excerpt)
  if (Object.keys(value).length > 0)
    return (
      <section aria-label={label}>
        <FieldList value={value} />
      </section>
    )
  if (excerpt.offset_bytes !== '0' || excerpt.continuation != null)
    return <p>{label} excerpt available in Raw</p>
  try {
    const parsed: unknown = JSON.parse(excerpt.text)
    if (
      parsed !== null &&
      typeof parsed === 'object' &&
      !Array.isArray(parsed) &&
      Object.keys(parsed).length === 0
    )
      return (
        <section aria-label={label}>
          <p>No fields</p>
        </section>
      )
    return (
      <section aria-label={label}>
        <p>
          {typeof parsed === 'string'
            ? `Text · ${Array.from(parsed).length} characters`
            : typeof parsed === 'number'
              ? 'Number available in Raw'
              : previewText(valueSummary(parsed)).content}
        </p>
        <small>Open Raw for the original text</small>
      </section>
    )
  } catch {
    return /^\s*[[{"]/u.test(excerpt.text) ? (
      <p>{label} details available in Raw</p>
    ) : (
      <TextPreview text={excerpt.text} label={label} />
    )
  }
}

function Result({ result, resultExcerpt }: Pick<RendererProps, 'result' | 'resultExcerpt'>) {
  return Object.keys(result).length > 0 ? (
    <FieldList value={result} />
  ) : (
    <Excerpt excerpt={resultExcerpt} label="Output" />
  )
}

function Command({ arguments: args, result, resultExcerpt }: RendererProps) {
  const argv = Array.isArray(args.arguments)
    ? args.arguments.map((arg) => JSON.stringify(arg)).join(' ')
    : ''
  const command = [typeof args.program === 'string' ? JSON.stringify(args.program) : '', argv]
    .filter(Boolean)
    .join(' ')
  const outcome = fields(result.outcome)
  const confinement = fields(result.confinement)
  const code = outcome.code
  const stdout = fields(result.stdout)
  const stderr = fields(result.stderr)
  return (
    <>
      <TextPreview text={command} label="Command" />
      {typeof args.timeout_seconds === 'number' && (
        <small>Timeout: {args.timeout_seconds} seconds</small>
      )}
      {textField(args.working_directory) && <small>In {textField(args.working_directory)}</small>}
      {typeof code === 'number' && <strong>Exit {code}</strong>}
      {typeof outcome.kind === 'string' && (
        <span>
          {outcome.kind === 'exited' && outcome.code === null
            ? 'Terminated by signal'
            : fieldLabel(outcome.kind)}
        </span>
      )}
      {typeof outcome.reason === 'string' && <span>{fieldLabel(outcome.reason)}</span>}
      {typeof confinement.kind === 'string' && <span>{fieldLabel(confinement.kind)}</span>}
      {typeof confinement.availability === 'string' && (
        <small>Sandbox availability: {fieldLabel(confinement.availability)}</small>
      )}
      {typeof result.diagnostic === 'string' && <small>{fieldLabel(result.diagnostic)}</small>}
      <TextPreview text={textField(stdout.text)} label="Output" />
      <TextPreview text={textField(stderr.text)} label="Error output" />
      {(stdout.completeness === 'truncated' || stderr.completeness === 'truncated') && (
        <small>Output was trimmed</small>
      )}
      {(stdout.encoding === 'lossy_utf8' || stderr.encoding === 'lossy_utf8') && (
        <small>Some output bytes could not be decoded</small>
      )}
      {!Object.keys(result).length && <Excerpt excerpt={resultExcerpt} label="Output" />}
    </>
  )
}

function FileRead({ arguments: args, result, resultExcerpt }: RendererProps) {
  return (
    <>
      <strong>{textField(args.path ?? result.path)}</strong>
      {typeof args.offset === 'number' && <small>Starting byte: {args.offset}</small>}
      {typeof args.max_bytes === 'number' && <small>Maximum bytes: {args.max_bytes}</small>}
      {typeof result.content === 'string' ? (
        result.content === '' ? (
          <p>No content in this read</p>
        ) : (
          <TextPreview text={result.content} label="File contents" />
        )
      ) : (
        <Result result={result} resultExcerpt={resultExcerpt} />
      )}
      {(result.truncated === true || (typeof result.offset === 'number' && result.offset > 0)) && (
        <small>Showing part of the file</small>
      )}
    </>
  )
}

function MediaRead({ arguments: args, result, resultExcerpt }: RendererProps) {
  return (
    <>
      <strong>Read attachment</strong>
      {typeof args.view === 'string' && <small>View: {fieldLabel(args.view)}</small>}
      {args.options && <FieldList value={fields(args.options)} />}
      {typeof args.continuation === 'string' && <small>Continue previous read</small>}
      {result.status === 'read' ? (
        <p>{result.output === 'image' ? 'Image returned' : 'Document returned'}</p>
      ) : result.status === 'text' && typeof result.body === 'string' ? (
        result.body === '' ? (
          <p>No content in this read</p>
        ) : (
          <TextPreview text={result.body} label="File contents" />
        )
      ) : result.status === 'structured' ? (
        <section aria-label="File contents">
          {result.body !== null &&
          typeof result.body === 'object' &&
          !Array.isArray(result.body) ? (
            Object.keys(result.body).length === 0 ? (
              <p>No fields</p>
            ) : (
              <FieldList value={fields(result.body)} />
            )
          ) : (
            <p>{previewText(valueSummary(result.body)).content}</p>
          )}
        </section>
      ) : typeof result.status === 'string' ? (
        <p>{fieldLabel(result.status)}</p>
      ) : null}
      {!Object.keys(result).length && <Excerpt excerpt={resultExcerpt} label="Output" />}
      {result.truncated === true && <small>Showing part of the file</small>}
    </>
  )
}

function FileEdit({ arguments: args, result, resultExcerpt }: RendererProps) {
  const old = textField(args.old_string)
  const replacement = textField(args.new_string ?? args.content)
  const patch =
    textField(args.patch ?? result.patch) ||
    [
      old
        ? previewText(old)
            .content.split('\n')
            .map((line) => `- ${line}`)
            .join('\n')
        : '',
      replacement
        ? previewText(replacement)
            .content.split('\n')
            .map((line) => `+ ${line}`)
            .join('\n')
        : '',
    ]
      .filter(Boolean)
      .join('\n')
  return (
    <>
      <strong>{textField(args.path ?? result.path)}</strong>
      <TextPreview text={patch} label="Proposed changes" diff />
      {args.content === '' && <strong>Write empty file</strong>}
      {args.replace_all === true && <strong>Replace every match</strong>}
      {(previewText(old).omittedCharacters > 0 ||
        previewText(replacement).omittedCharacters > 0) && (
        <small>Showing part of the changes</small>
      )}
      <Result result={result} resultExcerpt={resultExcerpt} />
    </>
  )
}

function Link({ url, title }: { url: unknown; title?: unknown }) {
  const href = webLink(url)
  return href ? (
    <a href={href} target="_blank" rel="noreferrer">
      {textField(title) || href}
    </a>
  ) : (
    <span>{textField(title) || textField(url)}</span>
  )
}

function Web({ arguments: args, result, resultExcerpt }: RendererProps) {
  return (
    <>
      <Link url={result.url ?? args.url} />
      {typeof result.status === 'number' && <strong>HTTP {result.status}</strong>}
      {typeof result.content_type === 'string' && (
        <small>Content type: {result.content_type}</small>
      )}
      {textField(args.query) && <strong>{textField(args.query)}</strong>}
      <TextPreview text={textField(result.body ?? result.summary)} label="Summary" />
      {Array.isArray(result.results) && result.results.length === 0 && <p>No results returned</p>}
      {Array.isArray(result.results) && result.results.length > 0 && (
        <ul>
          {result.results.map((item, index) => {
            const entry = fields(item)
            return (
              // biome-ignore lint/suspicious/noArrayIndexKey: Search result positions are immutable and URLs can repeat.
              <li key={index}>
                <Link url={entry.url} title={searchResultText(entry.title)} />
                <TextPreview text={searchResultText(entry.snippet)} label="Summary" />
              </li>
            )
          })}
        </ul>
      )}
      {!Object.keys(result).length && <Excerpt excerpt={resultExcerpt} label="Output" />}
      {result.truncated === true && <small>Showing part of the response</small>}
    </>
  )
}

function Git({ arguments: args, result, resultExcerpt }: RendererProps) {
  return (
    <>
      <FieldList value={args} />
      {typeof result.url === 'string' && <Link url={result.url} title={result.title} />}
      {typeof result.patch === 'string' ? (
        <>
          <TextPreview text={result.patch} label="Diff" diff />
          {result.truncated === true && <small>Showing part of the diff</small>}
        </>
      ) : (
        <Result result={result} resultExcerpt={resultExcerpt} />
      )}
    </>
  )
}

function Fallback({ arguments: args, result, resultExcerpt }: RendererProps) {
  return (
    <>
      <FieldList value={args} />
      <Result result={result} resultExcerpt={resultExcerpt} />
    </>
  )
}

export const toolRenderers: ReadonlyMap<string, ComponentType<RendererProps>> = new Map([
  ...['sandboxed_exec', 'unsandboxed_exec'].map((name) => [name, Command] as const),
  ['file_read', MediaRead],
  ...['read_file'].map((name) => [name, FileRead] as const),
  ...['write_file', 'edit_file', 'apply_patch'].map((name) => [name, FileEdit] as const),
  ...['web_fetch', 'web_search'].map((name) => [name, Web] as const),
  ...[
    'git_status',
    'git_diff',
    'git_log',
    'git_stage',
    'git_create_commit',
    'git_branch_create',
    'git_branch_switch',
    'git_push_configured',
    'github_pull_request_metadata',
    'github_pull_request_diff',
    'github_pull_request_review_threads',
    'github_pull_request_publish_review',
    'github_pull_request_create',
  ].map((name) => [name, Git] as const),
])

export function ToolCall({ tool, showMedia = true }: ToolCallProps) {
  const [raw, setRaw] = useState(false)
  const evidence = tool.evidence.type === 'physical_attempt' ? tool.evidence : null
  const args = excerptFields(tool.arguments)
  const result = excerptFields(evidence?.result)
  const Renderer =
    !evidence || evidence.cause === 'invalid_arguments' || evidence.cause === 'unknown_tool'
      ? Fallback
      : (toolRenderers.get(tool.tool_name) ?? Fallback)
  return (
    <article className="tool-call" aria-label={`Tool ${tool.tool_name}`}>
      <header>
        <strong>{fieldLabel(tool.tool_name)}</strong>
        <span>{evidence ? enumLabel(evidence.state) : 'Requested'}</span>
        {evidence?.cause && <span>{enumLabel(evidence.cause)}</span>}
        <button type="button" aria-expanded={raw} onClick={() => setRaw(!raw)}>
          Raw
        </button>
      </header>
      {raw ? (
        <>
          <Excerpt excerpt={tool.arguments} label="Arguments" expandable />
          <Excerpt excerpt={evidence?.result} label="Output" expandable />
        </>
      ) : (
        <>
          <Renderer arguments={args} result={result} resultExcerpt={evidence?.result} />
          {tool.arguments && Object.keys(args).length === 0 && (
            <Excerpt excerpt={tool.arguments} label="Arguments" />
          )}
        </>
      )}
      {!tool.arguments && <small>Arguments are on another detail page</small>}
      {evidence?.result_present && !evidence.result && (
        <small>Output is on another detail page</small>
      )}
      {evidence?.failure_present && !evidence.failure && (
        <small>Failure details are on another detail page</small>
      )}
      <Excerpt excerpt={evidence?.failure} label="Failure" expandable={raw} />
      {showMedia && <ToolResultMedia media={evidence?.result_media_reference} />}
    </article>
  )
}

export function ToolApproval({
  approval,
}: {
  approval: Extract<WebSessionTimelineDetailBody, { type: 'tool_approval_decision' }>
}) {
  const actors = {
    policy: 'Policy',
    user: 'User',
    user_override: 'User override',
    delegate: 'Approval reviewer',
  }
  return (
    <article className="tool-call" aria-label="Tool approval">
      <header>
        <strong>
          {approval.decision === 'approve' ? 'Approved' : 'Denied'} ·{' '}
          {fieldLabel(approval.tool_name)}
        </strong>
        <span>{actors[approval.actor.type]}</span>
      </header>
      <Excerpt excerpt={approval.rationale} label="Reason" expandable />
    </article>
  )
}
