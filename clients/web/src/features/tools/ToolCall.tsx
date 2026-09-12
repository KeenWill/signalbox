import { type ComponentType, useState } from 'react'
import type {
  WebSessionTimelineDetailBody,
  WebTimelineTextExcerpt,
  WebTimelineToolAttempt,
} from '../../generated/web-contract.mjs'
import { enumLabel } from '../../labels'
import {
  excerptFields,
  type Fields,
  fieldLabel,
  fields,
  previewText,
  textField,
  webLink,
} from './toolPresentation'
import './tools.css'

export interface ToolCallProps {
  tool: WebTimelineToolAttempt
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
}: {
  text: string
  label: string
  diff?: boolean
}) {
  const preview = previewText(text)
  if (!text) return null
  return (
    <section aria-label={label}>
      <pre className={diff ? 'tool-diff' : undefined}>
        <code>
          {diff
            ? preview.content.split('\n').map((line, index) => (
                <span
                  // biome-ignore lint/suspicious/noArrayIndexKey: Immutable lines can repeat.
                  key={index}
                  data-change={
                    line.startsWith('+') ? 'added' : line.startsWith('-') ? 'removed' : undefined
                  }
                >
                  {line}
                  {'\n'}
                </span>
              ))
            : preview.content}
        </code>
      </pre>
      {preview.omittedCharacters > 0 && <small>Showing part of the text</small>}
    </section>
  )
}

function Excerpt({ excerpt, label }: { excerpt?: WebTimelineTextExcerpt | null; label: string }) {
  if (!excerpt) return null
  return (
    <>
      <TextPreview text={excerpt.text} label={label} />
      {(excerpt.offset_bytes !== '0' || excerpt.continuation != null) && (
        <small>Showing part of {label.toLowerCase()}</small>
      )}
    </>
  )
}

function FieldList({ value }: { value: Fields }) {
  const preview = previewText(JSON.stringify(value))
  // A large payload stays in the bounded text view instead of mounting every field.
  if (preview.omittedCharacters > 0)
    return <TextPreview text={JSON.stringify(value)} label="Details" />
  return (
    <dl className="tool-fields">
      {Object.entries(value).map(([key, item]) => (
        <div key={key}>
          <dt>{fieldLabel(key)}</dt>
          <dd>{textField(item) || (item === null ? 'None' : JSON.stringify(item))}</dd>
        </div>
      ))}
    </dl>
  )
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
  const command =
    textField(args.command ?? args.cmd) || [textField(args.program), argv].filter(Boolean).join(' ')
  const outcome = fields(result.outcome)
  const code = result.exit_code ?? outcome.code
  const stdout = fields(result.stdout)
  const stderr = fields(result.stderr)
  return (
    <>
      <TextPreview text={command} label="Command" />
      {textField(args.working_directory ?? args.workdir) && (
        <small>In {textField(args.working_directory ?? args.workdir)}</small>
      )}
      {typeof code === 'number' && <strong>Exit {code}</strong>}
      {typeof outcome.kind === 'string' && <span>{fieldLabel(textField(outcome.kind))}</span>}
      <TextPreview text={textField(stdout.text ?? result.stdout ?? result.output)} label="Output" />
      <TextPreview text={textField(stderr.text ?? result.stderr)} label="Error output" />
      {(stdout.completeness === 'truncated' || stderr.completeness === 'truncated') && (
        <small>Output was trimmed</small>
      )}
      {!Object.keys(result).length && <Excerpt excerpt={resultExcerpt} label="Output" />}
    </>
  )
}

function FileRead({ arguments: args, result, resultExcerpt }: RendererProps) {
  return (
    <>
      <strong>{textField(args.path ?? args.file_path ?? result.path)}</strong>
      {typeof result.content === 'string' ? (
        <TextPreview text={result.content} label="File contents" />
      ) : (
        <Result result={result} resultExcerpt={resultExcerpt} />
      )}
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
      <strong>{textField(args.path ?? args.file_path ?? result.path)}</strong>
      <TextPreview text={patch} label="Proposed changes" diff />
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
      {textField(args.query) && <strong>{textField(args.query)}</strong>}
      <TextPreview text={textField(result.body ?? result.summary)} label="Summary" />
      {Array.isArray(result.results) && (
        <ul>
          {result.results.map((item, index) => {
            const entry = fields(item)
            return (
              // biome-ignore lint/suspicious/noArrayIndexKey: Search result positions are immutable and URLs can repeat.
              <li key={index}>
                <Link url={entry.url} title={entry.title} />
                <TextPreview text={textField(entry.snippet)} label="Summary" />
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
      <strong>
        {[
          textField(args.repository),
          args.number == null ? '' : `#${textField(args.number)}`,
          textField(args.title ?? args.message ?? args.branch ?? args.name ?? result.branch),
        ]
          .filter(Boolean)
          .join(' · ')}
      </strong>
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
  ...['read_file', 'list_directory', 'glob_files', 'search_files'].map(
    (name) => [name, FileRead] as const,
  ),
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

export function ToolCall({ tool }: ToolCallProps) {
  const [raw, setRaw] = useState(false)
  const evidence = tool.evidence.type === 'physical_attempt' ? tool.evidence : null
  const args = excerptFields(tool.arguments)
  const result = excerptFields(evidence?.result)
  const Renderer = toolRenderers.get(tool.tool_name) ?? Fallback
  return (
    <article className="tool-call" aria-label={`Tool ${tool.tool_name}`}>
      <header>
        <strong>{fieldLabel(tool.tool_name)}</strong>
        <span>{evidence ? enumLabel(evidence.state) : 'Requested'}</span>
        <button type="button" aria-expanded={raw} onClick={() => setRaw(!raw)}>
          Raw
        </button>
      </header>
      {raw ? (
        <>
          <Excerpt excerpt={tool.arguments} label="Arguments" />
          <Excerpt excerpt={evidence?.result} label="Output" />
        </>
      ) : (
        <>
          <Renderer arguments={args} result={result} resultExcerpt={evidence?.result} />
          {tool.arguments && Object.keys(args).length === 0 && (
            <Excerpt excerpt={tool.arguments} label="Arguments" />
          )}
        </>
      )}
      <Excerpt excerpt={evidence?.failure} label="Failure" />
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
    user: 'You',
    user_override: 'You (override)',
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
      <Excerpt excerpt={approval.rationale} label="Reason" />
    </article>
  )
}
