import { useQuery } from '@tanstack/react-query'
import { AlertTriangle, ArrowRight, Search } from 'lucide-react'
import { type FormEvent, type ReactNode, useEffect, useRef, useState } from 'react'
import type { WebContractBootstrap, WebSearchPage } from './generated/web-contract.mjs'
import { enumLabel } from './labels'
import {
  boundedSearchText,
  ProductRequestError,
  type ProductSearchState,
  ProductTransportError,
  productTransport,
} from './product'

type SearchResult = WebSearchPage['results'][number]

const MAX_U64 = 18_446_744_073_709_551_615n
const MAX_I64 = 9_223_372_036_854_775_807n
const MAX_SESSION_DRAFT_LENGTH = 45

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
  const [queryOverflow, setQueryOverflow] = useState(false)
  const [sessionOverflow, setSessionOverflow] = useState(false)
  const [draftIsInvalid, setDraftIsInvalid] = useState(false)
  const resultsHeadingRef = useRef<HTMLHeadingElement>(null)
  const errorHeadingRef = useRef<HTMLHeadingElement>(null)
  const routeValidationRef = useRef<HTMLParagraphElement>(null)
  const restoreResultsFocusRef = useRef(false)
  const submittedRouteChangeRef = useRef(false)
  const activeAfter =
    state.afterAddress && state.afterProjection
      ? { address: state.afterAddress, projectionId: state.afterProjection }
      : undefined
  const routeStateRef = useRef({
    queryParameterIsValid: state.queryParameterIsValid,
    q: state.q,
    session: state.session,
    sessionParameterIsValid: state.sessionParameterIsValid,
    afterAddress: state.afterAddress,
    afterProjection: state.afterProjection,
    cursorParametersAreValid: state.cursorParametersAreValid,
  })
  useEffect(() => {
    setDraftQuery(state.q ?? '')
    setQueryOverflow(state.q !== undefined && state.queryParameterIsValid === false)
  }, [state.q, state.queryParameterIsValid])
  useEffect(() => {
    setDraftSession(state.session ?? '')
    setSessionOverflow(state.session !== undefined && state.sessionParameterIsValid === false)
  }, [state.session, state.sessionParameterIsValid])
  useEffect(() => {
    const previous = routeStateRef.current
    const routeChanged =
      previous.queryParameterIsValid !== state.queryParameterIsValid ||
      previous.q !== state.q ||
      previous.session !== state.session ||
      previous.sessionParameterIsValid !== state.sessionParameterIsValid ||
      previous.afterAddress !== state.afterAddress ||
      previous.afterProjection !== state.afterProjection ||
      previous.cursorParametersAreValid !== state.cursorParametersAreValid
    if (routeChanged) setDraftIsInvalid(false)
    if (routeChanged && submittedRouteChangeRef.current) {
      submittedRouteChangeRef.current = false
    } else if (routeChanged) {
      restoreResultsFocusRef.current = !document.activeElement?.closest('.search-form')
    }
    routeStateRef.current = {
      queryParameterIsValid: state.queryParameterIsValid,
      q: state.q,
      session: state.session,
      sessionParameterIsValid: state.sessionParameterIsValid,
      afterAddress: state.afterAddress,
      afterProjection: state.afterProjection,
      cursorParametersAreValid: state.cursorParametersAreValid,
    }
  }, [
    state.afterAddress,
    state.afterProjection,
    state.cursorParametersAreValid,
    state.q,
    state.queryParameterIsValid,
    state.session,
    state.sessionParameterIsValid,
  ])
  const queryText = state.q?.trim() ?? ''
  const queryBytes = new TextEncoder().encode(queryText).length
  const queryLimit = bootstrap?.limits.max_search_query_bytes ?? 0
  const sessionIsValid =
    state.sessionParameterIsValid !== false &&
    (state.session === undefined || validUuid(state.session))
  const cursorMetadataIsValid =
    state.cursorParametersAreValid !== false &&
    (state.afterAddress === undefined && state.afterProjection === undefined
      ? true
      : state.afterAddress !== undefined && state.afterProjection !== undefined
        ? validCursor({ address: state.afterAddress, projectionId: state.afterProjection })
        : false)
  const requestIsValid =
    state.queryParameterIsValid !== false &&
    queryBytes > 0 &&
    queryBytes <= queryLimit &&
    !queryText.includes('\0') &&
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
    enabled:
      bootstrap?.capabilities.bounded_json === true &&
      bootstrap.capabilities.bounded_lexical_search === true &&
      requestIsValid,
    gcTime: 0,
  })
  const searchData = requestIsValid && !results.isError ? results.data : undefined
  const searchIsFetching = requestIsValid && results.isFetching
  const routeValidationIsVisible =
    bootstrap !== undefined &&
    (Boolean(queryText) || state.queryParameterIsValid === false) &&
    !requestIsValid
  useEffect(() => {
    if (!restoreResultsFocusRef.current) return
    if (document.activeElement?.closest('.search-form')) {
      restoreResultsFocusRef.current = false
      return
    }
    if (searchData !== undefined) {
      restoreResultsFocusRef.current = false
      resultsHeadingRef.current?.focus()
    } else if (requestIsValid && results.isError) {
      restoreResultsFocusRef.current = false
      errorHeadingRef.current?.focus()
    } else if (routeValidationIsVisible) {
      restoreResultsFocusRef.current = false
      routeValidationRef.current?.focus()
    }
  }, [requestIsValid, results.isError, routeValidationIsVisible, searchData])

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const q = String(form.get('q') ?? '').trim()
    const session = String(form.get('session') ?? '').trim()
    const qBytes = new TextEncoder().encode(q).length
    const submittedSession = session || undefined
    const draftParametersAreValid =
      !queryOverflow &&
      !sessionOverflow &&
      qBytes > 0 &&
      qBytes <= queryLimit &&
      !q.includes('\0') &&
      (submittedSession === undefined || validUuid(submittedSession))
    if (!draftParametersAreValid) {
      setDraftIsInvalid(true)
      return
    }
    setDraftIsInvalid(false)
    restoreResultsFocusRef.current = false
    if (
      q !== queryText ||
      state.queryParameterIsValid === false ||
      submittedSession !== state.session ||
      !sessionIsValid ||
      activeAfter !== undefined ||
      !cursorMetadataIsValid
    ) {
      submittedRouteChangeRef.current = true
      onStateChange({
        q,
        queryParameterIsValid: undefined,
        session: submittedSession,
        sessionParameterIsValid: undefined,
        afterAddress: undefined,
        afterProjection: undefined,
        cursorParametersAreValid: undefined,
      })
    } else if (requestIsValid) {
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
              id="product-search-input"
              name="q"
              value={draftQuery}
              onChange={(event) => {
                const bounded = boundedSearchText(event.currentTarget.value, queryLimit)
                setDraftQuery(bounded.text)
                setQueryOverflow(bounded.overflow)
                setDraftIsInvalid(false)
              }}
              placeholder="Search"
              required
            />
          </span>
        </label>
        <label>
          <span>Session ID</span>
          <input
            name="session"
            value={draftSession}
            onChange={(event) => {
              setSessionOverflow(event.currentTarget.value.length > MAX_SESSION_DRAFT_LENGTH)
              setDraftSession(event.currentTarget.value.slice(0, MAX_SESSION_DRAFT_LENGTH))
              setDraftIsInvalid(false)
            }}
            placeholder="All sessions"
          />
        </label>
        <button type="submit" disabled={bootstrap === undefined}>
          Search
        </button>
      </form>
      {(draftIsInvalid || queryOverflow || sessionOverflow) && !routeValidationIsVisible && (
        <p className="search-notice" role="alert">
          Check your search.
        </p>
      )}
      {routeValidationIsVisible && (
        <p className="search-notice" ref={routeValidationRef} role="alert" tabIndex={-1}>
          Check your search. Size: {queryBytes}/{queryLimit} bytes.
        </p>
      )}
      <p className="sr-only" role="status" aria-live="polite" aria-atomic="true">
        {searchIsFetching
          ? results.isLoading
            ? 'Searching.'
            : 'Refreshing.'
          : searchData
            ? `${searchData.results.length} results loaded on this page.`
            : ''}
      </p>
      {searchIsFetching && (
        <p className="search-notice">{results.isLoading ? 'Searching…' : 'Refreshing…'}</p>
      )}
      {requestIsValid && results.isError && (
        <section className="surface-empty" role="alert">
          <AlertTriangle aria-hidden="true" />
          <div>
            <h2 ref={errorHeadingRef} tabIndex={-1}>
              Search failed
            </h2>
            <p>
              {results.error instanceof ProductRequestError
                ? `${results.error.response.error.code}: ${results.error.message}`
                : results.error instanceof ProductTransportError
                  ? results.error.message
                  : 'The server sent an unexpected response.'}
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
              <h2 id="search-results-heading" ref={resultsHeadingRef} tabIndex={-1}>
                {searchData.results.length} {searchData.results.length === 1 ? 'result' : 'results'}
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
                  onStateChange({
                    q: queryText,
                    session: state.session,
                    afterAddress: nextAfter.address,
                    afterProjection: nextAfter.projectionId,
                  })
                }}
              >
                Next <ArrowRight aria-hidden="true" />
              </button>
            )}
          </header>
          {searchData.results.length === 0 ? (
            <p className="search-notice">No results</p>
          ) : (
            <>
              {/* biome-ignore lint/a11y/noRedundantRoles: Safari/VoiceOver needs an explicit role when CSS removes markers. */}
              <ol role="list">
                {searchData.results.map((result) => (
                  <li
                    key={`${result.session_id}:${result.address.event_sequence}:${result.projection_id}`}
                  >
                    <div className="search-result-meta">
                      <span>{enumLabel(result.content_class)}</span>
                      <code>{result.address.event_sequence}</code>
                    </div>
                    <p>{highlightedSnippet(result)}</p>
                    <div className="search-result-footer">
                      <span>{result.session_id}</span>
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
