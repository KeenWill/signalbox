import { useQuery, useQueryClient } from '@tanstack/react-query'
import { X } from 'lucide-react'
import type { FormEvent, RefObject } from 'react'
import { ArtifactRenderer } from './features/artifacts/ArtifactRenderer'
import {
  type BlobDescriptorInput,
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

const errorMessage = (error: Error): string => {
  if (error instanceof ProductRequestError) {
    return `${error.response.error.code}: ${error.message}`
  }
  if (error instanceof ProductTransportError) return error.message
  return 'The descriptor response did not match the generated web contract.'
}

export function ArtifactInspector({
  available,
  digestInputRef,
  onClose,
  state,
  onStateChange,
}: {
  available: boolean
  digestInputRef?: RefObject<HTMLInputElement | null>
  onClose: () => void
  state: ArtifactInspectorState
  onStateChange: (state: ArtifactInspectorState) => void
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

  const resolveDescriptor = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    queryClient.removeQueries({ queryKey: descriptorQueryPrefix })
    onStateChange({
      ...state,
      request: {
        digest,
        mediaType,
        displayFilename: displayFilename || undefined,
        sequence: (request?.sequence ?? 0) + 1,
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
          <button type="button" onClick={() => void descriptor.refetch()}>
            Retry
          </button>
        </div>
      )}
      {descriptor.data && (
        <ArtifactRenderer key={descriptor.data.digest} descriptor={descriptor.data} compact />
      )}
    </div>
  )
}
