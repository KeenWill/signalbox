import { useQuery } from '@tanstack/react-query'
import { type BlobDescriptorInput, productTransport } from '../../product'

export const useArtifactDescriptor = (
  input: BlobDescriptorInput | null,
  expectedByteLength?: string,
) =>
  useQuery({
    queryKey: [
      'production',
      'blob-descriptor',
      input?.digest,
      input?.mediaType,
      input?.displayFilename,
      expectedByteLength,
    ],
    queryFn: async ({ signal }) => {
      if (!input) throw new Error('Attachment identity is required')
      const descriptor = await productTransport.readBlobDescriptor(input, signal)
      if (expectedByteLength !== undefined && descriptor.byte_length !== expectedByteLength)
        throw new TypeError('Attachment length does not match its descriptor')
      return descriptor
    },
    enabled: input !== null,
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: 0,
    retry: false,
  })

export const useBlobCapability = (enabled: boolean): boolean => {
  const query = useQuery({
    queryKey: ['production', 'bootstrap'],
    queryFn: ({ signal }) => productTransport.readBootstrap(signal),
    staleTime: Number.POSITIVE_INFINITY,
    enabled,
    select: (bootstrap) => bootstrap.capabilities.immutable_blob_content,
  })
  return query.data === true
}
