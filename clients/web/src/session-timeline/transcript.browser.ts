import { expect, test } from '../../e2e/fontTest'
import { transcriptFixture } from './transcript.fixture'

import '../../e2e/session-transcript.spec'

for (const sameTurn of [false, true]) {
  test(`keeps following new messages ${sameTurn ? 'within a turn' : 'across turns'} while reading at the end`, async ({
    page,
  }) => {
    let latest = 100000
    await page.route('**/api/**', (route) => {
      const url = new URL(route.request().url())
      const payload = transcriptFixture(url, latest)
      if (
        sameTurn &&
        url.pathname.endsWith('/timeline-detail') &&
        url.searchParams.get('first') === '100001'
      ) {
        const page = payload as import('../generated/web-contract.mjs').WebSessionTimelineDetailPage
        return route.fulfill({
          json: {
            ...page,
            items: page.items.map((item) => ({
              ...item,
              body: { ...item.body, turn_id: '00000000-0000-0000-0000-000000100000' },
            })),
          },
        })
      }
      return route.fulfill({ json: payload })
    })
    await page.goto('/src/session-timeline/transcript-scenario.html?live=true')
    const transcript = page.getByRole('region', { name: 'Session transcript', exact: true })
    await expect(transcript.getByText('Message 100000', { exact: true })).toBeInViewport()
    latest += 1
    await page.getByRole('button', { name: 'Advance transcript' }).click()
    await expect(transcript.getByText('Message 100001', { exact: true })).toBeInViewport()
  })
}
