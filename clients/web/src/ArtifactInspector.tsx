import { X } from 'lucide-react'
import { type Dispatch, type RefObject, type SetStateAction, useMemo } from 'react'
import type { CommandContext } from './commands'
import { ArtifactRenderer, selectImageView } from './features/artifacts/ArtifactRenderer'
import { selectBoundedOriginalView } from './features/artifacts/artifactScenario'
import type { ArtifactItem } from './features/artifacts/artifactTypes'
import { useArtifactDescriptor } from './features/artifacts/descriptorService'
import type { WebBlobDescriptor } from './generated/web-contract.mjs'
import {
  type BlobDescriptorInput,
  ProductInputError,
  ProductRequestError,
  ProductTransportError,
} from './product'

export interface ArtifactRequest extends BlobDescriptorInput {
  sequence: number
}

export const artifactResolutionId = ({
  digest,
  sequence,
}: Pick<ArtifactRequest, 'digest' | 'sequence'>): string =>
  `product-artifact:${String(sequence)}:${digest}`

export interface ArtifactInspectorState {
  digest: string
  mediaType: string
  displayFilename: string
  request: ArtifactRequest | null
}

export const emptyArtifactInspectorState: ArtifactInspectorState = {
  digest: '',
  mediaType: '',
  displayFilename: '',
  request: null,
}

// Resolution identities are allocated from a module-scoped counter rather than counted within the
// inspector: the inspector's state unmounts with its route (an operator detour through Scenario
// studio), while the original-load projection is mounted above the router and outlives it. A
// component-local count restarts at 1 after such a remount and recreates a previous resolution's
// artifact ID, so the renderer would inherit that ID's settled `loaded` state and fetch original
// bytes without the new resolution's explicit Load original. A module-scoped counter never reissues
// an identity for the lifetime of the store that records those loads.
let lastResolutionSequence = 0

export const nextResolutionSequence = (): number => {
  lastResolutionSequence += 1
  return lastResolutionSequence
}

export const attachmentTypeLabel = (mediaType?: string | null): string => {
  if (mediaType?.startsWith('image/')) return 'Image'
  if (mediaType?.startsWith('audio/')) return 'Audio'
  if (mediaType?.startsWith('video/')) return 'Video'
  if (mediaType === 'application/pdf') return 'PDF'
  if (mediaType?.startsWith('text/')) return 'Text file'
  return 'File'
}

// Project an operator-resolved descriptor into the typed artifact the shared renderer registry
// consumes. The identity carries the resolution sequence so a re-resolve mounts a fresh renderer
// with fresh original-load state instead of inheriting the previous resolution's settled state.
export const inspectedArtifact = (
  descriptor: WebBlobDescriptor,
  sequence: number,
): ArtifactItem => {
  const identity = {
    id: artifactResolutionId({ digest: descriptor.digest, sequence }),
    displayName:
      descriptor.display_filename[0] ?? attachmentTypeLabel(descriptor.declared_media_type),
  }
  return selectImageView(descriptor) !== undefined ||
    selectBoundedOriginalView(descriptor) !== undefined
    ? { ...identity, kind: 'image', source: { kind: 'signalbox_blob', descriptor } }
    : { ...identity, kind: 'blob', descriptor }
}

const errorMessage = (error: Error): string => {
  if (error instanceof ProductInputError) return error.message
  if (error instanceof ProductRequestError) {
    return `${error.response.error.code}: ${error.message}`
  }
  if (error instanceof ProductTransportError) return error.message
  return 'Unexpected daemon response.'
}

export function ArtifactInspector({
  available,
  commandContext,
  onClose,
  state,
}: {
  available: boolean
  commandContext: CommandContext
  digestInputRef?: RefObject<HTMLInputElement | null>
  onClose: () => void
  state: ArtifactInspectorState
  onStateChange: Dispatch<SetStateAction<ArtifactInspectorState>>
}) {
  const { request } = state
  const descriptor = useArtifactDescriptor(available ? request : null)

  const resolved = descriptor.data
  const sequence = request?.sequence ?? 0
  const artifact = useMemo(
    () => (resolved === undefined ? null : inspectedArtifact(resolved, sequence)),
    [resolved, sequence],
  )
  // The registry gates original loading on the invoking context, so the inspector admits exactly
  // the artifact it resolved, and only when the descriptor proves a bounded original.
  const rendererContext = useMemo<CommandContext>(
    () => ({
      ...commandContext,
      artifactPreviewIds: artifact === null ? [] : [artifact.id],
      artifactOriginalIds:
        artifact !== null &&
        artifact.kind === 'image' &&
        artifact.source.kind === 'signalbox_blob' &&
        selectBoundedOriginalView(artifact.source.descriptor) !== undefined
          ? [artifact.id]
          : [],
    }),
    [artifact, commandContext],
  )

  return (
    <div className="artifact-inspector">
      <header>
        <div>
          <h2>Attachment details</h2>
        </div>
        <button
          className="icon-button"
          type="button"
          aria-label="Close attachment details"
          onClick={onClose}
        >
          <X />
        </button>
      </header>
      {!available && <p role="status">Attachments unavailable</p>}
      {available && request === null && (
        <p>Select an attachment in a conversation to view its details.</p>
      )}
      {request !== null && descriptor.isPending && <p role="status">Loading attachment…</p>}
      {descriptor.isError && (
        <div className="artifact-request-error" role="alert">
          <strong>Artifact unavailable</strong>
          <span>{errorMessage(descriptor.error)}</span>
          {!(descriptor.error instanceof ProductInputError) && (
            <button
              type="button"
              onClick={(event) => {
                const opener = event.currentTarget
                let restoreFocus = document.activeElement === opener
                const recordBlur = () => {
                  queueMicrotask(() => {
                    if (opener.isConnected) restoreFocus = false
                  })
                }
                const recordPointerMove = () => {
                  restoreFocus = false
                }
                opener.addEventListener('blur', recordBlur)
                document.addEventListener('pointerdown', recordPointerMove)
                void descriptor.refetch().then((result) => {
                  opener.removeEventListener('blur', recordBlur)
                  document.removeEventListener('pointerdown', recordPointerMove)
                  if (result.isSuccess && restoreFocus) {
                    requestAnimationFrame(() => {
                      if (
                        document.activeElement === opener ||
                        (!opener.isConnected && document.activeElement === document.body)
                      )
                        opener
                          .closest('.artifact-inspector')
                          ?.querySelector<HTMLButtonElement>('button')
                          ?.focus()
                    })
                  }
                })
              }}
            >
              Retry
            </button>
          )}
        </div>
      )}
      {artifact !== null && (
        <>
          <span className="sr-only" role="status" aria-live="polite" aria-atomic="true">
            Found {artifact.displayName}
          </span>
          <ArtifactRenderer
            key={artifact.id}
            artifact={artifact}
            commandContext={rendererContext}
          />
        </>
      )}
    </div>
  )
}
