import { expect, it } from 'vitest'
import { HttpSessionCreationApi, SessionCreationRejected } from './newSession'
import { createdSessionFixture } from './newSession.fixture'

const request = {
  command_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c70',
  template_name: 'code-review',
  first_input: null,
}

it('posts the exact creation identity and template and admits a committed receipt', async () => {
  const api = new HttpSessionCreationApi(async (path, init) => {
    expect(path).toBe('/api/sessions')
    expect(init?.method).toBe('POST')
    expect(init?.credentials).toBe('same-origin')
    expect(JSON.parse(String(init?.body))).toEqual(request)
    return Response.json(createdSessionFixture, { status: 201 })
  })
  await expect(api.create(request)).resolves.toEqual(createdSessionFixture)
})

it('rejects a creation receipt whose summary names a different session', async () => {
  const api = new HttpSessionCreationApi(async () =>
    Response.json(
      {
        ...createdSessionFixture,
        session_id: request.command_id,
      },
      { status: 201 },
    ),
  )
  await expect(api.create(request)).rejects.toThrow('does not match')
})

it('does not treat a successful status without the creation acknowledgement as committed', async () => {
  const api = new HttpSessionCreationApi(async () => Response.json(createdSessionFixture))
  await expect(api.create(request)).rejects.toThrow('not confirmed')
})

it('distinguishes definite rejection from an unconfirmed creation', async () => {
  const error = {
    error: {
      kind: 'application',
      code: 'invalid_session_creation',
      message: 'Template is not configured.',
    },
  }
  await expect(
    new HttpSessionCreationApi(async () => Response.json(error, { status: 400 })).create(request),
  ).rejects.toBeInstanceOf(SessionCreationRejected)
  await expect(
    new HttpSessionCreationApi(async () => Response.json(error, { status: 503 })).create(request),
  ).rejects.not.toBeInstanceOf(SessionCreationRejected)
})
