import { useQuery, useQueryClient } from '@tanstack/react-query'
import { X } from 'lucide-react'
import { type Dispatch, type FormEvent, type RefObject, type SetStateAction, useMemo } from 'react'
import type { CommandContext } from './commands'
import { ArtifactRenderer } from './features/artifacts/ArtifactRenderer'
import { selectBoundedOriginalView } from './features/artifacts/artifactScenario'
import type { ArtifactItem } from './features/artifacts/artifactTypes'
import type { WebBlobDescriptor } from './generated/web-contract.mjs'
import {
  type BlobDescriptorInput,
  ProductInputError,
  ProductRequestError,
  ProductTransportError,
  productTransport,
} from './product'

export interface ArtifactRequest extends BlobDescriptorInput {
  sequence: number
}

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

const descriptorQueryPrefix = ['production', 'blob-descriptor'] as const

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
): ArtifactItem => {
  const identity = {
    id: `product-artifact:${String(sequence)}:${descriptor.digest}`,
    displayName: descriptor.display_filename[0] ?? descriptor.digest,
  }
  return descriptor.declared_media_type.toLowerCase().startsWith('image/')
    ? { ...identity, kind: 'image', source: { kind: 'signalbox_blob', descriptor } }
    : { ...identity, kind: 'blob', descriptor }
}

const errorMessage = (error: Error): string => {
  if (error instanceof ProductInputError) return error.message
  if (error instanceof ProductRequestError) {
    return `${error.response.error.code}: ${error.message}`
  }
  if (error instanceof ProductTransportError) return error.message
  return 'The descriptor response did not match the generated web contract.'
}

export function ArtifactInspector({
  available,
  commandContext,
  digestInputRef,
  onClose,
  state,
  onStateChange,
}: {
  available: boolean
  commandContext: CommandContext
  digestInputRef?: RefObject<HTMLInputElement | null>
  onClose: () => void
  state: ArtifactInspectorState
  onStateChange: Dispatch<SetStateAction<ArtifactInspectorState>>
}) {
  const queryClient = useQueryClient()
  const { digest, mediaType, displayFilename, request } = state
  const descriptor = useQuery({
    queryKey: request
      ? [...descriptorQueryPrefix, request.sequence, request.digest]
      : [...descriptorQueryPrefix, 'idle'],
    queryFn: ({ signal }) => {
      if (!request) throw new Error('Artifact descriptor query started without an identity')
      return productTransport.readBlobDescriptor(request, signal)
    },
    enabled: request !== null && available,
    staleTime: Number.POSITIVE_INFINITY,
  })

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

  const resolveDescriptor = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    queryClient.removeQueries({ queryKey: descriptorQueryPrefix })
    onStateChange({
      ...state,
      request: {
        digest,
        mediaType,
        displayFilename: displayFilename || undefined,
        sequence: nextResolutionSequence(),
      },
    })
  }

  return (
    <div className="artifact-inspector">
      <header>
        <div>
          <span className="eyebrow">Immutable evidence</span>
          <h2>Artifact inspector</h2>
        </div>
        <button
          className="icon-button"
          type="button"
          aria-label="Close artifact inspector"
          onClick={onClose}
        >
          <X />
        </button>
      </header>
      <p>
        Resolve a blob identity already supplied by Signalbox. The browser loads only descriptor
        metadata and an admitted preview until you request original bytes.
      </p>
      {!available ? (
        <div className="artifact-capability" role="status">
          Blob delivery is unavailable in this daemon runtime.
        </div>
      ) : (
        <form onSubmit={resolveDescriptor}>
          <label>
            Digest
            <input
              ref={digestInputRef}
              name="digest"
              value={digest}
              onChange={(event) => onStateChange({ ...state, digest: event.target.value })}
              placeholder="sha256:…"
              pattern="sha256:[0-9a-f]{64}"
              autoComplete="off"
              required
            />
          </label>
          <label>
            Declared media type
            <input
              name="media-type"
              value={mediaType}
              onChange={(event) => onStateChange({ ...state, mediaType: event.target.value })}
              placeholder="image/png"
              autoComplete="off"
              required
            />
          </label>
          <label>
            Display filename <span>optional</span>
            <input
              name="display-filename"
              value={displayFilename}
              onChange={(event) => onStateChange({ ...state, displayFilename: event.target.value })}
              placeholder="evidence.png"
              autoComplete="off"
            />
          </label>
          <button type="submit" disabled={descriptor.isFetching}>
            {descriptor.isFetching ? 'Resolving…' : 'Resolve descriptor'}
          </button>
        </form>
      )}
      {descriptor.isError && (
        <div className="artifact-request-error" role="alert">
          <strong>Artifact unavailable</strong>
          <span>{errorMessage(descriptor.error)}</span>
          {!(descriptor.error instanceof ProductInputError) && (
            <button
              type="button"
              onClick={(event) => {
                const restoreFocus = document.activeElement === event.currentTarget
                void descriptor.refetch().then((result) => {
                  if (result.isSuccess && restoreFocus) {
                    requestAnimationFrame(() => digestInputRef?.current?.focus())
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
            Resolved artifact {artifact.displayName}
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
