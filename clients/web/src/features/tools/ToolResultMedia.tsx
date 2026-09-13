import { AttachmentReferences } from '../../AttachmentReferences'
import type { WebTimelineToolMediaReference } from '../../generated/web-contract.mjs'

export function ToolResultMedia({ media }: { media?: WebTimelineToolMediaReference | null }) {
  if (!media) return null
  return (
    <AttachmentReferences
      attachments={[
        { blob_id: media.digest, length_bytes: media.length_bytes, media_type: media.media_type },
      ]}
    />
  )
}
