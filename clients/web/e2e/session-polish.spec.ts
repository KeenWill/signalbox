import { expect, test } from './fontTest'
import {
  excerpt,
  initialMessage,
  openSession,
  sessionApi,
  sessionId,
  turnId,
} from './session-fixture'

test('composer fits an unchanged draft after the window narrows and widens', async ({ page }) => {
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(error.message))
  await page.setViewportSize({ width: 1440, height: 900 })
  await sessionApi(page)
  await openSession(page)
  const message = page.getByRole('textbox', { name: 'Message', exact: true })
  const draft = 'Keep this draft while resizing the window. '.repeat(8)
  await message.fill(draft)
  const wideHeight = (await message.boundingBox())?.height ?? 0
  await page.setViewportSize({ width: 390, height: 900 })
  await expect
    .poll(async () => (await message.boundingBox())?.height ?? 0)
    .toBeGreaterThan(wideHeight)
  await expect(message).toHaveValue(draft)
  await page.setViewportSize({ width: 1440, height: 900 })
  await expect.poll(async () => (await message.boundingBox())?.height).toBe(wideHeight)
  expect(errors).toEqual([])
})

for (const width of [1440, 390]) {
  test(`a short viewport can reach the header, conversation, and delivery status at ${width}`, async ({
    page,
  }, testInfo) => {
    await page.setViewportSize({ width, height: 160 })
    await sessionApi(page, true)
    await page.goto(`/sessions?workspace=true&session=${sessionId}`)
    const status = page.getByRole('form', { name: 'Message composer' }).getByRole('status')
    await expect(status).toHaveText('Wait for the current turn to finish · Running')
    await page.screenshot({ path: testInfo.outputPath(`session-short-${width}.png`) })
    const latest = page.getByRole('button', { name: 'Latest', exact: true })
    await latest.click()
    await expect(latest).toBeInViewport({ ratio: 1 })
    await page.getByText('Session details', { exact: true }).click()
    await page.keyboard.press('Escape')
    const message = page
      .getByRole('region', { name: 'Conversation', exact: true })
      .getByText(initialMessage)
    await message.scrollIntoViewIfNeeded()
    await expect(message).toBeInViewport({ ratio: 1 })
    await page.getByRole('region', { name: 'Session workspace', exact: true }).hover({
      position: { x: 2, y: 2 },
    })
    await expect(async () => {
      await page.mouse.wheel(0, 500)
      await expect(status).toBeInViewport({ ratio: 1, timeout: 500 })
    }).toPass()
  })

  test(`long conversation keeps its header and composer visible at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 })
    await sessionApi(page)
    const text = Array.from({ length: 80 }, (_, index) => `Conversation line ${index + 1}`).join(
      '\n',
    )
    await page.route(`**/api/sessions/${sessionId}/timeline-detail?**`, (route) => {
      const url = new URL(route.request().url())
      const includesMessage =
        BigInt(url.searchParams.get('first') ?? '0') <= 41n &&
        BigInt(url.searchParams.get('through') ?? '43') >= 41n
      return route.fulfill({
        json: {
          session_id: sessionId,
          projected_body_bytes: includesMessage ? 128 + text.length : 0,
          items: includesMessage
            ? [
                {
                  address: { event_sequence: '41' },
                  kind: 'input_accepted',
                  projected_body_bytes: 128 + text.length,
                  body: {
                    type: 'user_input',
                    turn_id: turnId,
                    text: excerpt(text),
                    attachments: [],
                  },
                },
              ]
            : [],
          continuation: null,
        },
      })
    })
    await page.goto(`/sessions?workspace=true&session=${sessionId}`)
    const conversation = page.getByRole('region', { name: 'Conversation', exact: true })
    await expect(conversation.getByText(text)).toBeVisible()
    await conversation.hover()
    await page.mouse.wheel(0, 1600)
    await expect(page.getByRole('button', { name: 'Latest', exact: true })).toBeInViewport()
    const message = page.getByRole('textbox', { name: 'Message', exact: true })
    await expect(message).toBeInViewport()
    await message.fill(text)
    expect((await message.boundingBox())?.height).toBeLessThanOrEqual(225)
    await expect(page.getByRole('button', { name: 'Send message', exact: true })).toBeInViewport()
    await message.fill('')
    expect((await message.boundingBox())?.height).toBeLessThanOrEqual(34)
  })
  for (const state of ['idle', 'running', 'network', 'recovery', 'repository'] as const) {
    test(`session layout ${state} at ${width}`, async ({ page }, testInfo) => {
      await page.setViewportSize({ width, height: 900 })
      const api = await sessionApi(
        page,
        state === 'running' || state === 'network',
        sessionId,
        state === 'repository'
          ? {
              dispatch_id: turnId,
              action_ordinal: '1',
              repository: 'signalbox/example',
              head_branch: 'session-polish',
              base_branch: 'main',
              pull_request: '81',
              rule_id: 'review-response',
              rule_revision: '3',
              event_id: sessionId,
              event_kind: 'review_submitted',
            }
          : null,
      )
      if (state === 'recovery')
        api.state.supervision = {
          class: 'infrastructure',
          cause_code: 'infrastructure',
          pending: true,
        }
      if (state === 'network')
        api.state.activeState = {
          kind: 'awaiting_credential_availability',
          wait_attempt_id: turnId,
          cause: 'network_unavailable',
        }
      await openSession(page)
      await page.screenshot({ path: testInfo.outputPath(`session-${state}-${width}.png`) })
      const composer = page.getByRole('form', { name: 'Message composer' })
      const conversation = page.getByRole('region', { name: 'Conversation', exact: true })
      await expect(composer).toBeInViewport()
      await expect(conversation).toBeInViewport()
      const composerBox = await composer.boundingBox()
      const conversationBox = await conversation.boundingBox()
      const workspaceBox = await page
        .getByRole('region', { name: 'Session workspace', exact: true })
        .boundingBox()
      expect(composerBox?.height).toBeLessThan(100)
      expect(conversationBox?.height).toBeGreaterThan((workspaceBox?.height ?? 0) / 2)
      await page.getByText('Session details', { exact: true }).click()
      await expect(page.getByRole('button', { name: 'Previous', exact: true })).toHaveCount(0)
      await expect(page.getByRole('button', { name: 'Next', exact: true })).toHaveCount(0)
    })
  }
}
