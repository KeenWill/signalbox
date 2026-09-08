import { useQuery, useQueryClient } from '@tanstack/react-query'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { fetchVerifiedSingleFrameJpeg } from './artifactScenario'

type BlobView = WebBlobDescriptor['available_views'][number]

// Typed client service owning the verified-original request: transport, cancellation, caching,
// and response state live here, keyed by the immutable content URL, so a renderer only subscribes
// to the projection. Content is digest-addressed and never stale; a remounted renderer restores a
// loaded original from cache, or through one bounded refetch after the global cache bound evicts
// it, instead of losing it to component-local state.
export const useVerifiedOriginalImage = (view: BlobView | undefined, requested: boolean) => {
  const client = useQueryClient()
  const queryKey = ['artifact-original', view?.content_url ?? null]
  const query = useQuery({
    queryKey,
    queryFn: ({ signal }) => {
      if (view === undefined) throw new Error('an admitted original view is required')
      return fetchVerifiedSingleFrameJpeg(
        view,
        (input, init) => fetch(input, { ...init, cache: 'reload' }),
        signal,
      )
    },
    enabled: requested && view !== undefined,
    staleTime: Number.POSITIVE_INFINITY,
    retry: false,
  })
  return { ...query, discard: () => client.removeQueries({ queryKey, exact: true }) }
}
