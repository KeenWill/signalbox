import { HttpSearchUsageSource } from './model'

export const usageSourceOptions = {
  queryKey: ['usage-http-source'],
  queryFn: () => HttpSearchUsageSource.connectUsage(),
  staleTime: Infinity,
}
