import { X } from 'lucide-react'
import { useMemo } from 'react'
import type { CommandContext } from './commands'
import { ArtifactRenderer, selectImageView } from './features/artifacts/ArtifactRenderer'
import { selectBoundedOriginalView } from './features/artifacts/artifactScenario'
import type { ArtifactItem } from './features/artifacts/artifactTypes'
import { useArtifactDescriptor } from './features/artifacts/descriptorService'
import type { WebBlobDescriptor } from './generated/web-contract.mjs'
import { attachmentTypeLabel } from './labels'
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

// Project an operator-resolved descriptor into the typed artifact the shared renderer registry
// consumes. The identity carries the resolution sequence so a re-resolve mounts a fresh renderer
// with fresh original-load state instead of inheriting the previous resolution's settled state.
export const inspectedArtifact = (
  descriptor: WebBlobDescriptor,
  sequence: number,
  presentationKind?: 'image' | 'document',
): ArtifactItem => {
  const identity = {
    id: artifactResolutionId({ digest: descriptor.digest, sequence }),
    displayName:
      descriptor.display_filename[0] ??
      attachmentTypeLabel(descriptor.declared_media_type, presentationKind),
  }
  if (presentationKind === 'document')
    return {
      ...identity,
      kind: 'document',
      source: { kind: 'signalbox_blob', descriptor },
      documentKind:
        descriptor.declared_media_type.split(';', 1)[0]?.toLowerCase() === 'application/pdf'
          ? 'pdf'
          : 'document',
    }
  return presentationKind === 'image' ||
    selectImageView(descriptor) !== undefined ||
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
  request,
  presentationKind,
  expectedByteLength,
}: {
  available: boolean
  presentationKind?: 'image' | 'document'
  expectedByteLength?: string
  commandContext: CommandContext
  onClose: () => void
  request: ArtifactRequest | null
}) {
  const descriptor = useArtifactDescriptor(available ? request : null, expectedByteLength)

  const resolved = descriptor.data
  const sequence = request?.sequence ?? 0
  const artifact = useMemo(
    () => (resolved === undefined ? null : inspectedArtifact(resolved, sequence, presentationKind)),
    [resolved, sequence, presentationKind],
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
                const focusScope = opener.closest('[role=dialog]')
                const focusTarget = opener
                  .closest('.artifact-inspector')
                  ?.querySelector<HTMLButtonElement>('button')
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
                        (!opener.isConnected &&
                          (document.activeElement === document.body ||
                            document.activeElement === focusScope))
                      )
                        focusTarget?.focus()
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
