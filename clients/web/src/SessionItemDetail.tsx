import { useQuery } from '@tanstack/react-query'
import { type ReactNode, useEffect, useRef, useState } from 'react'
import type {
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import {
  type HttpSessionTimelineSource,
  type TimelineDetailIdentity,
  timelineDetailIdentity,
} from './session-timeline/model'

const DETAIL_PAGE_ITEMS = 1
const DETAIL_PAGE_BYTES = 16 * 1024
const MAX_RENDERED_ATTACHMENTS = 24

type DetailItem = WebSessionTimelineDetailPage['items'][number]
type DetailBody = DetailItem['body']
type TextExcerpt = Extract<DetailBody, { type: 'user_input' }>['text']
type ModelCallBody = Extract<DetailBody, { type: 'model_call' }>

export const detailBodyMatchesKind = (detail: DetailItem): boolean => {
  const body = detail.body
  if (body.type === 'user_input') return detail.kind === 'input_accepted'
  if (body.type === 'model_call') return detail.kind === 'model_call_transition'
  if (body.type === 'event_fact') {
    return (
      body.kind === detail.kind &&
      ![
        'input_accepted',
        'model_call_transition',
        'turn_activated',
        'turn_completed',
        'turn_failed',
        'turn_refused',
        'turn_cancelled',
        'turn_reconciliation_required',
      ].includes(detail.kind)
    )
  }
  if (body.lifecycle === 'activated') {
    return detail.kind === 'turn_activated' && body.cause_code === 'activated'
  }
  const terminalCauseByKind: Partial<Record<DetailItem['kind'], string>> = {
    turn_completed: 'completed',
    turn_failed: 'failed',
    turn_refused: 'refused',
    turn_cancelled: 'cancelled',
    turn_reconciliation_required: 'reconciliation_required',
  }
  return terminalCauseByKind[detail.kind] === body.cause_code
}

const modelCallState = (state: ModelCallBody['state']): string =>
  state.type === 'terminal' ? `terminal · ${state.disposition}` : state.type.replaceAll('_', ' ')

const TextDetail = ({ label, excerpt }: { label: string; excerpt: TextExcerpt }) => {
  return (
    <section className="session-detail-text" aria-label={label}>
      <header>
        <strong>{label}</strong>
        <span>
          offset {excerpt.offset_bytes} B · total {excerpt.total_bytes} B
        </span>
      </header>
      <pre>{excerpt.text}</pre>
    </section>
  )
}

const DetailRecord = ({ detail }: { detail: DetailItem }) => {
  const body = detail.body
  let content: ReactNode
  if (body.type === 'user_input') {
    const visibleAttachments = body.attachments.slice(0, MAX_RENDERED_ATTACHMENTS)
    content = (
      <>
        <TextDetail label="User input" excerpt={body.text} />
        {visibleAttachments.length > 0 && (
          <ul className="session-detail-attachments" aria-label="Attachment references">
            {visibleAttachments.map((attachment) => (
              <li key={attachment.blob_id}>
                <code>{attachment.blob_id}</code>
                <span>
                  {attachment.media_type ?? 'unknown media'} · {attachment.length_bytes} B
                </span>
              </li>
            ))}
          </ul>
        )}
        {body.attachments.length > MAX_RENDERED_ATTACHMENTS && (
          <p className="session-detail-note">
            Showing {MAX_RENDERED_ATTACHMENTS} of {body.attachments.length} attachment references.
          </p>
        )}
      </>
    )
  } else if (body.type === 'model_call') {
    content = (
      <>
        <dl className="session-detail-facts">
          <div>
            <dt>Turn</dt>
            <dd>{body.turn_id}</dd>
          </div>
          <div>
            <dt>Model call</dt>
            <dd>{body.model_call_id}</dd>
          </div>
          <div>
            <dt>Model</dt>
            <dd>{body.model_identity_id}</dd>
          </div>
          <div>
            <dt>State</dt>
            <dd>{modelCallState(body.state)}</dd>
          </div>
          <div>
            <dt>Cause</dt>
            <dd>{body.cause_code ?? 'not reported'}</dd>
          </div>
          <div>
            <dt>Request context items</dt>
            <dd>{body.request_context_items}</dd>
          </div>
          <div>
            <dt>Input tokens</dt>
            <dd>{body.usage.input_tokens ?? 'not reported'}</dd>
          </div>
          <div>
            <dt>Output tokens</dt>
            <dd>{body.usage.output_tokens ?? 'not reported'}</dd>
          </div>
          <div>
            <dt>Cache creation input tokens</dt>
            <dd>{body.usage.cache_creation_input_tokens ?? 'not reported'}</dd>
          </div>
          <div>
            <dt>Cache read input tokens</dt>
            <dd>{body.usage.cache_read_input_tokens ?? 'not reported'}</dd>
          </div>
        </dl>
        {body.response ? (
          <TextDetail label="Model response" excerpt={body.response} />
        ) : (
          <p className="session-detail-note">No response text was recorded at this checkpoint.</p>
        )}
      </>
    )
  } else if (body.type === 'turn_lifecycle') {
    content = (
      <dl className="session-detail-facts">
        <div>
          <dt>Turn</dt>
          <dd>{body.turn_id}</dd>
        </div>
        <div>
          <dt>Lifecycle</dt>
          <dd>{body.lifecycle}</dd>
        </div>
        <div>
          <dt>Cause</dt>
          <dd>{body.cause_code}</dd>
        </div>
      </dl>
    )
  } else {
    content = (
      <p className="session-detail-note">Typed event fact · {body.kind.replaceAll('_', ' ')}</p>
    )
  }

  return (
    <article className="session-detail-record">
      <div className="session-detail-record-heading">
        <strong>{detail.kind.replaceAll('_', ' ')}</strong>
        <span>{detail.projected_body_bytes} projected B</span>
      </div>
      {content}
    </article>
  )
}

export function SessionItemDetail({
  source,
  sessionId,
  item,
}: {
  source: HttpSessionTimelineSource
  sessionId: string
  item: WebSessionTimelineWindow['items'][number]
}) {
  const [cursor, setCursor] = useState<NonNullable<WebSessionTimelineDetailPage['continuation']>>()
  const [expectedTotalBytes, setExpectedTotalBytes] = useState<string>()
  const [expectedIdentity, setExpectedIdentity] = useState<TimelineDetailIdentity>()
  const retryRef = useRef<HTMLButtonElement>(null)
  const detailRef = useRef<HTMLElement>(null)
  const detail = useQuery({
    queryKey: [
      'production',
      'session-item-detail',
      sessionId,
      item.address.event_sequence,
      cursor,
      expectedTotalBytes,
    ],
    queryFn: ({ signal }) =>
      source.readItemDetail(
        sessionId,
        item.address.event_sequence,
        { maxItems: DETAIL_PAGE_ITEMS, maxBytes: DETAIL_PAGE_BYTES },
        cursor,
        signal,
        expectedTotalBytes,
        expectedIdentity,
      ),
    gcTime: 0,
    placeholderData: (previousData) => previousData,
  })
  useEffect(() => {
    if (detail.isError && cursor) retryRef.current?.focus()
  }, [cursor, detail.isError])
  useEffect(() => {
    if (cursor && detail.data && !detail.isFetching && detail.data.continuation === null) {
      detailRef.current?.focus()
    }
  }, [cursor, detail.data, detail.isFetching])

  if (detail.isError) {
    return (
      <div className="session-detail-state" role="alert">
        <p>Detail unavailable: {detail.error.message}</p>
        <button ref={retryRef} type="button" onClick={() => void detail.refetch()}>
          Retry typed detail
        </button>
      </div>
    )
  }
  if (!detail.data) return <p className="session-detail-state">Loading typed detail…</p>
  if (
    detail.data.items.some(
      (detailItem) => detailItem.kind !== item.kind || !detailBodyMatchesKind(detailItem),
    )
  ) {
    return (
      <p className="session-detail-state" role="alert">
        Detail rejected because its event kind did not match the selected timeline header.
      </p>
    )
  }

  return (
    <section
      ref={detailRef}
      className="session-item-detail"
      tabIndex={-1}
      aria-label="Loaded typed detail chunk"
    >
      {detail.data.items.map((detailItem) => (
        <DetailRecord
          key={`${detailItem.address.event_sequence}:${detailItem.body.type}`}
          detail={detailItem}
        />
      ))}
      {detail.data.items.length === 0 && (
        <p className="session-detail-note">No typed body was returned for this bounded page.</p>
      )}
      {detail.data.continuation && (
        <button
          type="button"
          className="session-detail-continue"
          disabled={detail.isFetching}
          onClick={() => {
            const continuation = detail.data.continuation
            if (continuation?.type !== 'more_body') return
            const detailItem = detail.data.items.find(
              (candidate) =>
                candidate.address.event_sequence === continuation.body.address.event_sequence,
            )
            const excerpt =
              continuation.body.field === 'input_text' && detailItem?.body.type === 'user_input'
                ? detailItem.body.text
                : continuation.body.field === 'model_response' &&
                    detailItem?.body.type === 'model_call'
                  ? detailItem.body.response
                  : undefined
            if (!excerpt || !detailItem) return
            setExpectedTotalBytes(excerpt.total_bytes)
            setExpectedIdentity(timelineDetailIdentity(detailItem))
            setCursor(continuation)
          }}
        >
          {detail.isFetching
            ? 'Loading next bounded detail chunk…'
            : 'Load next bounded detail chunk'}
        </button>
      )}
    </section>
  )
}
