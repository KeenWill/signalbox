import { useQuery } from '@tanstack/react-query'
import { AlertTriangle, ArrowRight, Search } from 'lucide-react'
import { type FormEvent, type ReactNode, useEffect, useRef, useState } from 'react'
import type { WebContractBootstrap, WebSearchPage } from './generated/web-contract.mjs'
import {
  ProductRequestError,
  type ProductSearchState,
  ProductTransportError,
  productTransport,
} from './product'

type SearchResult = WebSearchPage['results'][number]

const displayClass = (value: string) => value.replaceAll('_', ' ')
const MAX_U64 = 18_446_744_073_709_551_615n
const MAX_I64 = 9_223_372_036_854_775_807n

const validUuid = (value: string) => {
  const simple = /^[0-9a-f]{32}$/i
  const hyphenated = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
  return (
    simple.test(value) ||
    hyphenated.test(value) ||
    (value.startsWith('{') && value.endsWith('}') && hyphenated.test(value.slice(1, -1))) ||
    (/^urn:uuid:/i.test(value) && hyphenated.test(value.slice(9)))
  )
}

const validPositiveDecimal = (value: string, maximum: bigint) =>
  /^[1-9][0-9]*$/.test(value) && value.length <= 20 && BigInt(value) <= maximum

const validCursor = (after: { address: string; projectionId: string } | undefined) =>
  after === undefined ||
  (validPositiveDecimal(after.address, MAX_U64) &&
    validPositiveDecimal(after.projectionId, MAX_I64))

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
  const resultsHeadingRef = useRef<HTMLHeadingElement>(null)
  const restoreResultsFocusRef = useRef(false)
  const [activeAfter, setActiveAfter] = useState(() =>
    state.afterAddress && state.afterProjection
      ? { address: state.afterAddress, projectionId: state.afterProjection }
      : undefined,
  )
  useEffect(() => setDraftQuery(state.q ?? ''), [state.q])
  useEffect(() => setDraftSession(state.session ?? ''), [state.session])
  useEffect(
    () =>
      setActiveAfter(
        state.afterAddress && state.afterProjection
          ? { address: state.afterAddress, projectionId: state.afterProjection }
          : undefined,
      ),
    [state.afterAddress, state.afterProjection],
  )
  useEffect(() => {
    const synchronizeWithHistory = () => {
      const search = new URLSearchParams(window.location.search)
      const address = search.get('afterAddress')
      const projectionId = search.get('afterProjection')
      setActiveAfter(address && projectionId ? { address, projectionId } : undefined)
    }
    window.addEventListener('popstate', synchronizeWithHistory)
    return () => window.removeEventListener('popstate', synchronizeWithHistory)
  }, [])
  const queryText = state.q?.trim() ?? ''
  const queryBytes = new TextEncoder().encode(queryText).length
  const queryLimit = bootstrap?.limits.max_search_query_bytes ?? 0
  const sessionIsValid = state.session === undefined || validUuid(state.session)
  const routeSearch = new URLSearchParams(window.location.search)
  const routeAfterAddress = routeSearch.get('afterAddress') ?? undefined
  const routeAfterProjection = routeSearch.get('afterProjection') ?? undefined
  const cursorMetadataIsValid =
    routeAfterAddress === undefined && routeAfterProjection === undefined
      ? true
      : routeAfterAddress !== undefined && routeAfterProjection !== undefined
        ? validCursor({ address: routeAfterAddress, projectionId: routeAfterProjection })
        : false
  const requestIsValid =
    queryBytes > 0 &&
    queryBytes <= queryLimit &&
    sessionIsValid &&
    cursorMetadataIsValid &&
    validCursor(activeAfter)
  const results = useQuery({
    queryKey: ['production', 'search', queryText, state.session ?? null, activeAfter ?? null],
    queryFn: ({ signal }) =>
      productTransport.search(
        {
          query: queryText,
          sessionId: state.session,
          maxItems: Math.min(100, bootstrap?.limits.max_search_page_items ?? 1),
          maxSnippetBytes: bootstrap?.limits.max_search_snippet_bytes ?? 0,
          after: activeAfter,
        },
        signal,
      ),
    enabled: bootstrap?.capabilities.bounded_lexical_search === true && requestIsValid,
    gcTime: 0,
  })
  const searchData = requestIsValid ? results.data : undefined
  useEffect(() => {
    if (restoreResultsFocusRef.current && searchData !== undefined) {
      restoreResultsFocusRef.current = false
      resultsHeadingRef.current?.focus()
    }
  }, [searchData])

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const q = String(form.get('q') ?? '').trim()
    const session = String(form.get('session') ?? '').trim()
    restoreResultsFocusRef.current = false
    setActiveAfter(undefined)
    onStateChange({ q: q || undefined, session: session || undefined })
    if (
      requestIsValid &&
      activeAfter === undefined &&
      q === queryText &&
      (session || undefined) === state.session
    ) {
      void results.refetch()
    }
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
      {bootstrap !== undefined && queryText && !requestIsValid && (
        <p className="search-notice" role="alert">
          Search parameters are malformed or outside the contract bounds. Search text uses{' '}
          {queryBytes} of {queryLimit} allowed UTF-8 bytes.
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
        {requestIsValid && results.isLoading
          ? 'Searching the durable projection.'
          : searchData
            ? `${searchData.results.length} results loaded on this page.`
            : ''}
      </p>
      {requestIsValid && results.isLoading && (
        <p className="search-notice">Searching the durable projection…</p>
      )}
      {requestIsValid && results.isError && (
        <section className="surface-empty" role="alert">
          <AlertTriangle aria-hidden="true" />
          <div>
            <h2>Search could not be read</h2>
            <p>
              {results.error instanceof ProductRequestError
                ? `${results.error.code}: ${results.error.message}`
                : results.error instanceof ProductTransportError
                  ? results.error.message
                  : 'The response did not match the generated web contract.'}
            </p>
            <button
              type="button"
              onClick={() => {
                restoreResultsFocusRef.current = true
                void results.refetch()
              }}
            >
              Retry
            </button>
          </div>
        </section>
      )}
      {searchData && (
        <section className="search-results" aria-labelledby="search-results-heading">
          <header>
            <div>
              <span className="eyebrow">Newest logical address first</span>
              <h2 id="search-results-heading" ref={resultsHeadingRef} tabIndex={-1}>
                {searchData.results.length} results on this page
              </h2>
            </div>
            {searchData.continuation && (
              <button
                type="button"
                onClick={() => {
                  const continuation = searchData.continuation
                  if (continuation == null) return
                  const nextAfter = {
                    address: continuation.address.event_sequence,
                    projectionId: continuation.projection_id,
                  }
                  restoreResultsFocusRef.current = true
                  setActiveAfter(nextAfter)
                  onStateChange({
                    q: queryText,
                    session: state.session,
                    afterAddress: nextAfter.address,
                    afterProjection: nextAfter.projectionId,
                  })
                }}
              >
                Next page <ArrowRight aria-hidden="true" />
              </button>
            )}
          </header>
          {searchData.results.length === 0 ? (
            <p className="search-notice">No indexed durable text matched this query.</p>
          ) : (
            <>
              {/* biome-ignore lint/a11y/noRedundantRoles: Safari/VoiceOver needs an explicit role when CSS removes markers. */}
              <ol role="list">
                {searchData.results.map((result) => (
                  <li
                    key={`${result.session_id}:${result.address.event_sequence}:${result.projection_id}`}
                  >
                    <div className="search-result-meta">
                      <span>{displayClass(result.content_class)}</span>
                      <code>{result.address.event_sequence}</code>
                    </div>
                    <p>{highlightedSnippet(result)}</p>
                    <div className="search-result-footer">
                      <span>{result.session_id}</span>
                      <span>Session reveal unavailable</span>
                    </div>
                  </li>
                ))}
              </ol>
            </>
          )}
        </section>
      )}
    </div>
  )
}
