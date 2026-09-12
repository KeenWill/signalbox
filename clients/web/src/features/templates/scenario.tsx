import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
  RouterProvider,
} from '@tanstack/react-router'
import { createRoot } from 'react-dom/client'
import '../../app.css'
import { TemplatesRoute } from './TemplatesRoute'

const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
const rootRoute = createRootRoute({
  component: () => (
    <main style={{ height: '100dvh' }}>
      <Outlet />
    </main>
  ),
})
const templateRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/$surface',
  component: TemplatesRoute,
})
// The standalone fixture exercises the same URL and history transitions as the product mount.
window.history.replaceState(null, '', '/templates')
const router = createRouter({ routeTree: rootRoute.addChildren([templateRoute]) })
const root = document.getElementById('root')
if (root)
  createRoot(root).render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  )
