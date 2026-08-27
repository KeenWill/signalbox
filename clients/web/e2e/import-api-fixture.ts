import { createHash } from 'node:crypto'
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
      const sourceSessionId =
        segments[2] === 'searches'
          ? (request.postData() ?? undefined)
          : (url.searchParams.get('source_session_id') ?? undefined)
      const listRequest: WebImportListRequest = {
        after: url.searchParams.get('after') ?? undefined,
        format: (url.searchParams.get('format') as WebImportFormat | null) ?? undefined,
        limit: optionalNumber(url.searchParams.get('limit')),
        source_session_id: sourceSessionId,
      }
      const listPage = await api.list(listRequest)
      if (segments[2] === 'searches' && sourceSessionId !== undefined) {
        const digest = createHash('sha256').update(sourceSessionId).digest('hex')
        await route.fulfill({
          json: {
            ...listPage,
            items: listPage.items.map((item) => ({
              ...item,
              source_session_id_sha256: digest,
            })),
            search_correlation: url.searchParams.get('search_correlation'),
            exact_source_session_id_sha256: digest,
          },
        })
        return
      }
      await route.fulfill({ json: listPage })
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
