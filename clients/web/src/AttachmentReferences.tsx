import type { WebSessionTimelineDetailBody } from './generated/web-contract.mjs'

type Attachments = Extract<WebSessionTimelineDetailBody, { type: 'user_input' }>['attachments']

export function AttachmentReferences({ attachments }: { attachments: Attachments }) {
  if (attachments.length === 0) return null
  return (
    <ul className="session-detail-attachments" aria-label="Attachment references">
      {attachments.map((attachment) => (
        <li key={attachment.blob_id}>
          <code>{attachment.blob_id}</code>
          <span>
            {attachment.media_type ?? 'unknown media'} · {attachment.length_bytes} B
          </span>
        </li>
      ))}
    </ul>
  )
}
