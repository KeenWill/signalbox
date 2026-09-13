import { afterEach, expect, it, vi } from 'vitest'
import {
  HttpSessionCreationApi,
  readRetainedCreation,
  retainCreation,
  SessionCreationRejected,
} from './newSession'
import { createdSessionFixture } from './newSession.fixture'
import { MAX_PRODUCT_JSON_BYTES } from './product'

const request = {
  command_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c70',
  template_name: 'code-review',
  first_input: null,
}

afterEach(() => {
  sessionStorage.clear()
  vi.useRealTimers()
})

it('evicts an unreadable retained creation request', () => {
  sessionStorage.setItem('signalbox.new-session', '{')

  expect(() => readRetainedCreation()).toThrow()
  expect(readRetainedCreation()).toBeNull()
})

it('reads a valid retained creation request', () => {
  retainCreation(request)

  expect(readRetainedCreation()).toEqual(request)
})

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

it.each([201, 400, 503])(
  'cancels an advertised oversized creation response with status %s',
  async (status) => {
    let cancelled = false
    const body = new ReadableStream<Uint8Array>({
      cancel() {
        cancelled = true
      },
    })
    const api = new HttpSessionCreationApi(
      async () =>
        new Response(body, {
          status,
          headers: { 'content-length': String(MAX_PRODUCT_JSON_BYTES + 1) },
        }),
    )

    await expect(api.create(request)).rejects.toThrow('product JSON byte limit')
    expect(cancelled).toBe(true)
  },
)

it.each([201, 400, 503])(
  'stops an oversized streaming creation response with status %s',
  async (status) => {
    let cancelled = false
    let chunks = 0
    const body = new ReadableStream<Uint8Array>(
      {
        pull(controller) {
          chunks += 1
          controller.enqueue(new TextEncoder().encode(' '.repeat(MAX_PRODUCT_JSON_BYTES / 2)))
        },
        cancel() {
          cancelled = true
        },
      },
      { highWaterMark: 0 },
    )
    const api = new HttpSessionCreationApi(async () => new Response(body, { status }))

    await expect(api.create(request)).rejects.toThrow('product JSON byte limit')
    expect(cancelled).toBe(true)
    expect(chunks).toBe(3)
  },
)

it.each([null, 201, 400, 503])(
  'aborts stalled creation with response status %s and retries the retained identity',
  async (status) => {
    vi.useFakeTimers()
    retainCreation(request)
    const fetch = vi.fn<typeof globalThis.fetch>(async (_url, init) => {
      const signal = init?.signal
      if (status === null) {
        return new Promise<Response>((_resolve, reject) => {
          signal?.addEventListener('abort', () => reject(signal.reason), { once: true })
        })
      }
      return new Response(
        new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(new TextEncoder().encode('{'))
            signal?.addEventListener('abort', () => controller.error(signal.reason), { once: true })
          },
        }),
        { status },
      )
    })
    const api = new HttpSessionCreationApi(fetch)
    const outcome = expect(api.create(request)).rejects.not.toBeInstanceOf(SessionCreationRejected)
    await vi.advanceTimersByTimeAsync(30_000)
    await outcome
    expect(fetch.mock.calls[0]?.[1]?.signal?.aborted).toBe(true)
    expect(vi.getTimerCount()).toBe(0)
    expect(readRetainedCreation()).toEqual(request)
    fetch.mockResolvedValueOnce(Response.json(createdSessionFixture, { status: 201 }))
    await expect(api.create(readRetainedCreation() ?? request)).resolves.toEqual(
      createdSessionFixture,
    )
    expect(fetch.mock.calls[1]?.[1]?.body).toEqual(fetch.mock.calls[0]?.[1]?.body)
    expect(vi.getTimerCount()).toBe(0)
  },
)
