import { useQuery } from '@tanstack/react-query'
import { Link } from '@tanstack/react-router'
import { ArrowRight, Search } from 'lucide-react'
import { type FormEvent, type ReactNode, useEffect, useState } from 'react'
import type { WebContractBootstrap, WebSearchPage } from './generated/web-contract.mjs'
import { ProductRequestError, type ProductSearchState, productTransport } from './product'

type SearchResult = WebSearchPage['results'][number]

const displayClass = (value: string) => value.replaceAll('_', ' ')

const sourceIdentity = (result: SearchResult) => {
  const source = result.source
  switch (source.kind) {
    case 'session':
      return source.session_id
    case 'accepted_input':
      return source.accepted_input_id
    case 'turn_transcript_entry':
    case 'session_transcript_entry':
      return source.semantic_entry_id
    case 'tool_request':
      return source.tool_request_id
    case 'tool_attempt':
      return source.tool_attempt_id
    case 'attachment':
      return source.attachment_id
    case 'derived_artifact':
      return source.artifact_id
  }
}

function highlightedSnippet(result: SearchResult): ReactNode {
  const bytes = new TextEncoder().encode(result.snippet)
  const decoder = new TextDecoder('utf-8', { fatal: true })
  const parts: ReactNode[] = []
  let cursor = 0
  try {
    for (const highlight of result.highlights) {
      if (highlight.start_byte < cursor || highlight.end_byte > bytes.length) return result.snippet
      parts.push(decoder.decode(bytes.slice(cursor, highlight.start_byte)))
      parts.push(
        <mark key={`${highlight.start_byte}:${highlight.end_byte}`}>
          {decoder.decode(bytes.slice(highlight.start_byte, highlight.end_byte))}
        </mark>,
      )
      cursor = highlight.end_byte
    }
    parts.push(decoder.decode(bytes.slice(cursor)))
    return parts
  } catch {
    return result.snippet
  }
}

export function SearchSurface({
  bootstrap,
  state,
  onStateChange,
}: {
  bootstrap?: WebContractBootstrap
  state: ProductSearchState
  onStateChange: (state: ProductSearchState) => void
}) {
  const [draftQuery, setDraftQuery] = useState(state.q ?? '')
  const [draftSession, setDraftSession] = useState(state.session ?? '')
  useEffect(() => setDraftQuery(state.q ?? ''), [state.q])
  useEffect(() => setDraftSession(state.session ?? ''), [state.session])
  const queryText = state.q?.trim() ?? ''
  const queryBytes = new TextEncoder().encode(queryText).length
  const queryLimit = bootstrap?.limits.max_search_query_bytes ?? 0
  const requestIsValid = queryBytes > 0 && queryBytes <= queryLimit
  const after =
    state.afterAddress && state.afterProjection
      ? { address: state.afterAddress, projectionId: state.afterProjection }
      : undefined
  const results = useQuery({
    queryKey: ['production', 'search', queryText, state.session ?? null, after ?? null],
    queryFn: ({ signal }) =>
      productTransport.search(
        {
          query: queryText,
          sessionId: state.session,
          maxItems: Math.min(100, bootstrap?.limits.max_search_page_items ?? 1),
          maxSnippetBytes: bootstrap?.limits.max_search_snippet_bytes ?? 0,
          after,
        },
        signal,
      ),
    enabled: bootstrap?.capabilities.bounded_lexical_search === true && requestIsValid,
    gcTime: 0,
  })

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const q = String(form.get('q') ?? '').trim()
    const session = String(form.get('session') ?? '').trim()
    onStateChange({ q: q || undefined, session: session || undefined })
  }

  return (
    <div className="surface-body search-surface">
      <form className="search-form" onSubmit={submit}>
        <label>
          <span>Search text</span>
          <span className="search-input">
            <Search aria-hidden="true" />
            <input
              name="q"
              value={draftQuery}
              onChange={(event) => setDraftQuery(event.currentTarget.value)}
              placeholder="Natural language terms"
              required
            />
          </span>
        </label>
        <label>
          <span>
            Exact session <small>optional</small>
          </span>
          <input
            name="session"
            value={draftSession}
            onChange={(event) => setDraftSession(event.currentTarget.value)}
            placeholder="Session UUID"
          />
        </label>
        <button type="submit" disabled={bootstrap === undefined}>
          Search
        </button>
      </form>
      {queryText && !requestIsValid && (
        <p className="search-notice" role="alert">
          Search text uses {queryBytes} of {queryLimit} allowed UTF-8 bytes.
        </p>
      )}
      {!queryText && (
        <section className="search-zero">
          <span className="eyebrow">Bounded lexical index</span>
          <h2>Search durable text without loading transcripts</h2>
          <p>
            Results preserve their session, typed source, content class, and logical timeline
            address for a direct history reveal.
          </p>
        </section>
      )}
      <p className="sr-only" role="status" aria-live="polite" aria-atomic="true">
        {results.isLoading
          ? 'Searching the durable projection.'
          : results.data
            ? `${results.data.results.length} results loaded on this page.`
            : ''}
      </p>
      {results.isLoading && <p className="search-notice">Searching the durable projection…</p>}
      {results.isError && (
        <section className="surface-empty" role="alert">
          <div>
            <h2>Search could not be read</h2>
            <p>
              {results.error instanceof ProductRequestError
                ? `${results.error.code}: ${results.error.message}`
                : 'The response did not match the generated web contract.'}
            </p>
            <button type="button" onClick={() => void results.refetch()}>
              Retry
            </button>
          </div>
        </section>
      )}
      {results.data && (
        <section className="search-results" aria-labelledby="search-results-heading">
          <header>
            <div>
              <span className="eyebrow">Newest logical address first</span>
              <h2 id="search-results-heading">
                {results.data.results.length} results on this page
              </h2>
            </div>
            {results.data.continuation && (
              <button
                type="button"
                onClick={() =>
                  onStateChange({
                    q: queryText,
                    session: state.session,
                    afterAddress: results.data.continuation?.address.event_sequence,
                    afterProjection: results.data.continuation?.projection_id,
                  })
                }
              >
                Next page <ArrowRight aria-hidden="true" />
              </button>
            )}
          </header>
          {results.data.results.length === 0 ? (
            <p className="search-notice">No indexed durable text matched this query.</p>
          ) : (
            <ol>
              {results.data.results.map((result) => (
                <li
                  key={`${result.session_id}:${result.address.event_sequence}:${result.source.kind}:${result.content_class}:${sourceIdentity(result)}`}
                >
                  <div className="search-result-meta">
                    <span>{displayClass(result.content_class)}</span>
                    <code>{result.address.event_sequence}</code>
                  </div>
                  <p>{highlightedSnippet(result)}</p>
                  <div className="search-result-footer">
                    <span>{result.session_id}</span>
                    <Link
                      to="/$surface"
                      params={{ surface: 'sessions' }}
                      search={{ session: result.session_id, around: result.address.event_sequence }}
                    >
                      Reveal in session <ArrowRight aria-hidden="true" />
                    </Link>
                  </div>
                </li>
              ))}
            </ol>
          )}
        </section>
      )}
    </div>
  )
}
