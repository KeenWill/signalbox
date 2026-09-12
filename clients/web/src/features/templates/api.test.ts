import { describe, expect, it } from 'vitest'
import { HttpTemplateApi } from './api'
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
})
