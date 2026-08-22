import { useQuery } from '@tanstack/react-query'
import { type ReactNode, useState } from 'react'
import type {
  WebSessionTimelineDetailPage,
  WebSessionTimelineWindow,
} from './generated/web-contract.mjs'
import type { HttpSessionTimelineSource } from './session-timeline/model'

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
  if (body.type === 'event_fact') return body.kind === detail.kind
  if (body.lifecycle === 'activated') return detail.kind === 'turn_activated'
  return ['turn_completed', 'turn_failed', 'turn_refused', 'turn_cancelled'].includes(detail.kind)
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
            <dt>Model</dt>
            <dd>{body.model_identity_id}</dd>
          </div>
          <div>
            <dt>State</dt>
            <dd>{modelCallState(body.state)}</dd>
          </div>
          <div>
            <dt>Input tokens</dt>
            <dd>{body.usage.input_tokens ?? 'not reported'}</dd>
          </div>
          <div>
            <dt>Output tokens</dt>
            <dd>{body.usage.output_tokens ?? 'not reported'}</dd>
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
  const detail = useQuery({
    queryKey: ['production', 'session-item-detail', sessionId, item.address.event_sequence, cursor],
    queryFn: ({ signal }) =>
      source.readItemDetail(
        sessionId,
        item.address.event_sequence,
        { maxItems: DETAIL_PAGE_ITEMS, maxBytes: DETAIL_PAGE_BYTES },
        cursor,
        signal,
      ),
    gcTime: 0,
    placeholderData: (previousData) => previousData,
  })

  if (detail.isError) {
    return (
      <p className="session-detail-state" role="alert">
        Detail unavailable: {detail.error.message}
      </p>
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
    <div className="session-item-detail">
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
          onClick={() => setCursor(detail.data.continuation ?? undefined)}
        >
          {detail.isFetching
            ? 'Loading next bounded detail chunk…'
            : 'Load next bounded detail chunk'}
        </button>
      )}
    </div>
  )
}
