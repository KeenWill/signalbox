import { describe, expect, it, vi } from 'vitest'
import { MAX_PRODUCT_JSON_BYTES } from '../../product'
import { HttpTemplateApi, MAX_TEMPLATE_LIST_ITEMS } from './api'
import { detailFixture, templateFixture } from './fixtures'

describe('template API', () => {
  it('passes cancellation through the selected detail request', async () => {
    const controller = new AbortController()
    const api = new HttpTemplateApi(async (input, init) => {
      expect(input).toBe('/api/templates/code-review')
      expect(init?.signal).toBe(controller.signal)
      return Response.json(detailFixture)
    })
    expect((await api.detail(templateFixture.name, controller.signal)).summary.name).toBe(
      templateFixture.name,
    )
  })

  it('rejects a response for another template', async () => {
    const api = new HttpTemplateApi(async () => Response.json(detailFixture))
    await expect(api.detail('another-template')).rejects.toThrow('does not match')
  })

  it('rejects unknown workflow approval postures', async () => {
    const api = new HttpTemplateApi(async () =>
      Response.json({
        templates: [
          {
            ...templateFixture,
            workflow_tools: [{ ...templateFixture.workflow_tools[0], posture: 'unknown' }],
          },
        ],
      }),
    )
    await expect(api.list()).rejects.toThrow()
  })

  it('surfaces the daemon validation error', async () => {
    const api = new HttpTemplateApi(async () =>
      Response.json(
        {
          error: {
            kind: 'application',
            code: 'templates_unavailable',
            message: 'Templates are unavailable',
          },
        },
        { status: 503 },
      ),
    )
    await expect(api.list()).rejects.toThrow('Templates are unavailable')
  })
  it('puts the exact definition and rejects a save receipt for another template', async () => {
    const request = { definition_toml: detailFixture.definition_toml }
    const api = new HttpTemplateApi(async (input, init) => {
      expect(input).toBe('/api/templates/code-review')
      expect(init?.method).toBe('PUT')
      expect(init?.body).toBe(JSON.stringify(request))
      return Response.json({
        ...detailFixture,
        summary: { ...templateFixture, name: 'another-template' },
      })
    })
    await expect(api.save(templateFixture.name, request)).rejects.toThrow('does not match')
  })
})

// Keep the scale fixture compact enough to exercise the item bound independently of bytes.
const compactTemplate = { ...templateFixture, workflow_tools: [] }

it('admits a bounded catalog and forwards cancellation', async () => {
  const controller = new AbortController()
  const templates = Array.from({ length: MAX_TEMPLATE_LIST_ITEMS }, (_, index) => ({
    ...compactTemplate,
    name: `template-${index}`,
  }))
  const api = new HttpTemplateApi(async (input, init) => {
    expect(input).toBe('/api/templates')
    expect(init?.signal).toBe(controller.signal)
    return Response.json({ templates })
  })
  await expect(api.list(controller.signal)).resolves.toEqual({ templates })
})

it('rejects excessive catalog entries before decoding their contents', async () => {
  const api = new HttpTemplateApi(async () =>
    Response.json({ templates: Array.from({ length: MAX_TEMPLATE_LIST_ITEMS * 10 }, () => null) }),
  )
  await expect(api.list()).rejects.toThrow('item limit')
})

it.each([true, false])('cancels oversized catalog bodies (advertised: %s)', async (advertised) => {
  const cancel = vi.fn()
  const body = new ReadableStream({
    start(controller) {
      controller.enqueue(new Uint8Array(MAX_PRODUCT_JSON_BYTES + 1))
    },
    cancel,
  })
  const api = new HttpTemplateApi(
    async () =>
      new Response(body, {
        headers: advertised ? { 'content-length': String(MAX_PRODUCT_JSON_BYTES + 1) } : {},
      }),
  )
  await expect(api.list()).rejects.toThrow('JSON byte limit')
  expect(cancel).toHaveBeenCalledOnce()
})
