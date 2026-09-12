import { HttpSearchUsageSource } from './model'

export const usageSourceOptions = {
  queryKey: ['usage-http-source'],
  queryFn: () => HttpSearchUsageSource.connect(),
  staleTime: Infinity,
}
