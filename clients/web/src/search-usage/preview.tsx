import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRoot } from 'react-dom/client'
import '../app.css'
import { SearchUsageScenarioSource } from './scenario'
import { UsageContent } from './UsageSurface'

if (import.meta.env.DEV) {
  const root = document.getElementById('root')
  if (root)
    createRoot(root).render(
      <QueryClientProvider client={new QueryClient()}>
        <UsageContent source={new SearchUsageScenarioSource()} />
      </QueryClientProvider>,
    )
}
