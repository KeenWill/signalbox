import type { Page } from '@playwright/test'
import type {
  WebImportContinuationRequest,
  WebImportEntryWindowRequest,
  WebImportFormat,
  WebImportListRequest,
} from '../src/generated/web-contract.mjs'
import { ScenarioImportApi } from '../src/imports/scenario'

const optionalNumber = (value: string | null): number | undefined =>
  value === null ? undefined : Number(value)

export const useDeterministicImportApi = async (page: Page) => {
  const api = new ScenarioImportApi()
  await page.route('**/api/imports/**', async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const segments = url.pathname.split('/').filter(Boolean)
    const importedConversationId = decodeURIComponent(segments[2] ?? '')

    if (segments.length === 2 || (segments[2] === 'searches' && request.method() === 'POST')) {
      const listRequest: WebImportListRequest =
        segments[2] === 'searches'
          ? (request.postDataJSON() as WebImportListRequest)
          : {
              after: url.searchParams.get('after') ?? undefined,
              format: (url.searchParams.get('format') as WebImportFormat | null) ?? undefined,
              limit: optionalNumber(url.searchParams.get('limit')),
              source_session_id: url.searchParams.get('source_session_id') ?? undefined,
            }
      await route.fulfill({ json: await api.list(listRequest) })
      return
    }
    if (segments[3] === 'entries') {
      const entryRequest: WebImportEntryWindowRequest = {
        anchor:
          (url.searchParams.get('anchor') as WebImportEntryWindowRequest['anchor'] | null) ??
          undefined,
        position: optionalNumber(url.searchParams.get('position')),
        before: optionalNumber(url.searchParams.get('before')),
        after: optionalNumber(url.searchParams.get('after')),
      }
      await route.fulfill({ json: await api.entries(importedConversationId, entryRequest) })
      return
    }
    if (segments[3] === 'continuations') {
      const continuationRequest = request.postDataJSON() as WebImportContinuationRequest
      await route.fulfill({
        json: await api.continueImport(importedConversationId, continuationRequest),
      })
      return
    }
    await route.fulfill({ json: await api.descriptor(importedConversationId) })
  })
}
