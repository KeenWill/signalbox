import { useQuery } from '@tanstack/react-query'
import { type BlobDescriptorInput, productTransport } from '../../product'

export const useArtifactDescriptor = (input: BlobDescriptorInput | null) =>
  useQuery({
    queryKey: [
      'production',
      'blob-descriptor',
      input?.digest,
      input?.mediaType,
      input?.displayFilename,
    ],
    queryFn: ({ signal }) => {
      if (!input) throw new Error('Attachment identity is required')
      return productTransport.readBlobDescriptor(input, signal)
    },
    enabled: input !== null,
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: 0,
    retry: false,
  })
