import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  createRootRoute,
  createRoute,
  createRouter,
  parseSearchWith,
  RouterProvider,
  stringifySearchWith,
} from '@tanstack/react-router'
import { createRoot } from 'react-dom/client'
import '../app.css'
import { readProductRouteState } from '../product'
import { SearchUsageScenarioSource } from './scenario'
import { UsageContent, UsageSurface } from './UsageSurface'

const source = new SearchUsageScenarioSource()
const rootRoute = createRootRoute()
const route = createRoute({
  getParentRoute: () => rootRoute,
  path: '/src/search-usage/preview.html',
  validateSearch: readProductRouteState,
  component: () =>
    new URLSearchParams(window.location.search).has('http') ? (
      <UsageSurface />
    ) : (
      <UsageContent source={source} />
    ),
})
const router = createRouter({
  routeTree: rootRoute.addChildren([route]),
  parseSearch: parseSearchWith((value) => value),
  stringifySearch: stringifySearchWith(String),
})

if (import.meta.env.DEV) {
  const root = document.getElementById('root')
  if (root)
    createRoot(root).render(
      <QueryClientProvider client={new QueryClient()}>
        <RouterProvider router={router} />
      </QueryClientProvider>,
    )
}
