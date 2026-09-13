import { useQuery, useQueryClient } from '@tanstack/react-query'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { fetchVerifiedSingleFrameJpeg } from './artifactScenario'

type BlobView = WebBlobDescriptor['available_views'][number]

// Only requesting renderers observe original bytes; the last observer releases the cache.
export const useVerifiedOriginalImage = (view: BlobView | undefined, requested: boolean) => {
  const client = useQueryClient()
  const queryKey = ['artifact-original', requested ? (view?.content_url ?? null) : null]
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
    gcTime: 0,
  })
  return { ...query, discard: () => client.removeQueries({ queryKey, exact: true }) }
}
