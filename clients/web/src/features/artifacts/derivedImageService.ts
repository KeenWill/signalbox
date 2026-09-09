import { useQuery } from '@tanstack/react-query'
import { useEffect, useState } from 'react'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { fetchVerifiedDerivedImage } from './artifactScenario'

type BlobView = WebBlobDescriptor['available_views'][number]

export const useVerifiedDerivedImage = (view: BlobView | undefined) => {
  const query = useQuery({
    queryKey: ['artifact-derived', view?.content_url ?? null, view?.byte_length ?? null],
    queryFn: ({ signal }) => {
      if (view === undefined) throw new Error('an admitted derived view is required')
      return fetchVerifiedDerivedImage(view, fetch, signal)
    },
    enabled: view !== undefined,
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: 0,
    retry: false,
  })
  const blob = query.data
  const [object, setObject] = useState<{ blob: Blob; url: string } | null>(null)
  useEffect(() => {
    if (blob === undefined) return
    const url = URL.createObjectURL(blob)
    setObject({ blob, url })
    return () => URL.revokeObjectURL(url)
  }, [blob])
  return { url: object?.blob === blob ? object?.url : undefined, isError: query.isError }
}
