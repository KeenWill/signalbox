import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { useState } from 'react'
import { createRoot } from 'react-dom/client'
import '../../app.css'
import { TemplatesSurface } from './TemplatesSurface'

const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
function Scenario() {
  const [name, select] = useState<string>()
  return (
    <main style={{ height: '100dvh' }}>
      <TemplatesSurface selectedName={name} onSelect={select} />
    </main>
  )
}
const root = document.getElementById('root')
if (root)
  createRoot(root).render(
    <QueryClientProvider client={queryClient}>
      <Scenario />
    </QueryClientProvider>,
  )
