import { afterEach, describe, expect, it, vi } from 'vitest'
import { fallbackDescriptor, imageArtifact } from './features/artifacts/artifactScenario'
import {
  MAX_ATTENTION_SNAPSHOT_ITEMS,
  MAX_DECLARED_MEDIA_TYPE_BYTES,
  MAX_DISPLAY_FILENAME_BYTES,
  MAX_PRODUCT_JSON_BYTES,
  ProductContractError,
  ProductInputError,
  ProductRequestError,
  ProductTransportError,
  productRoutes,
  productSurfaceCacheLabel,
  productSurfaceStates,
  readProductRouteState,
  readProductSearchState,
  SameOriginProductTransport,
} from './product'
import { webContractBootstrapFixture } from './product.fixture'
import { hasValidSessionTimelineContract } from './session-timeline/model'

// The generated contract is authored in Rust; keep one fixture for every browser test.
const bootstrapFixture = webContractBootstrapFixture

const sessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
const previousSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6c'
const laterSessionId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6e'
const turnId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c70'
const attentionFixture = {
  continuation_after_session_id: null,
  cursor: '17',
  summaries: [
    {
      action: 'decide_approval',
      current_turn_id: turnId,
      goal_block: null,
      judge: { actionable: '2', completed: '7', escalated: '1', failed: '0' },
      last_activity: { kind: 'approval_judge', unix_milliseconds: '1724200000000' },
      lifecycle_state: 'waiting',
      session_id: sessionId,
      state: 'awaiting_approval',
    },
  ],
} as const
const attentionUpdateFixture = {
  kind: 'update',
  cursor: '18',
  summaries: [{ ...attentionFixture.summaries[0], state: 'active', action: null }],
} as const
const errorFixture = {
  error: {
    code: 'attention_projection_unavailable',
    kind: 'application',
    message: 'attention projection is not configured',
  },
} as const

const interruptedResponse = (): Response =>
  new Response(
    new ReadableStream({
      start(controller) {
        controller.error(new TypeError('response stream interrupted'))
      },
    }),
  )

const searchPageFixture = {
  results: [
    {
      session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d',
      address: { event_sequence: '901' },
      projection_id: '42',
      source: { kind: 'session', session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d' },
      content_class: 'session_metadata',
      snippet: 'search fixture',
      highlights: [{ start_byte: 0, end_byte: 6 }],
    },
  ],
  continuation: null,
} as const

const escapedSearchPageFixture = {
  results: Array.from({ length: 100 }, (_, index) => ({
    ...searchPageFixture.results[0],
    address: { event_sequence: String(100 - index) },
    snippet: '\0'.repeat(512),
    highlights: [],
  })),
  continuation: null,
}

const searchErrorFixture = {
  error: {
    code: 'search_projection_unavailable',
    kind: 'application',
    message: 'search projection is not configured',
  },
} as const

afterEach(() => vi.unstubAllGlobals())

describe('SameOriginProductTransport', () => {
  it('decodes the Rust-authored bootstrap contract', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(webContractBootstrapFixture))),
    )

    const bootstrap = await new SameOriginProductTransport().readBootstrap()

    expect(bootstrap).toEqual(webContractBootstrapFixture)
  })

  it('rejects bootstrap facts that contradict the fixed v2 contract', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...webContractBootstrapFixture,
              limits: { ...webContractBootstrapFixture.limits, max_ndjson_item_bytes: 262_144 },
            }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductContractError,
    )
  })

  it('accepts deployment-disabled blob capabilities', async () => {
    const bootstrap = {
      ...webContractBootstrapFixture,
      capabilities: {
        ...webContractBootstrapFixture.capabilities,
        immutable_blob_content: false,
        blob_derivations: false,
        image_derivatives: false,
      },
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(bootstrap))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).resolves.toEqual(bootstrap)
  })

  it('rejects contradictory blob capability dependencies', async () => {
    const bootstrap = {
      ...webContractBootstrapFixture,
      capabilities: {
        ...webContractBootstrapFixture.capabilities,
        blob_derivations: false,
        image_derivatives: true,
      },
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(bootstrap))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductContractError,
    )
  })

  it('fails closed when the daemon returns an unknown contract shape', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ invented: true }))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductContractError,
    )
  })

  it('rejects a bootstrap response beyond the JSON byte ceiling', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response('x'.repeat(MAX_PRODUCT_JSON_BYTES + 1), {
            headers: { 'content-type': 'application/json' },
          }),
      ),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductContractError,
    )
  })

  it('classifies a rejected fetch as a transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('network failed'))),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductTransportError,
    )
  })

  it('classifies an interrupted bootstrap response stream as a transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => interruptedResponse()),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductTransportError,
    )
  })

  it('rejects a mismatched bootstrap contract identity', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...bootstrapFixture,
              contract: {
                ...bootstrapFixture.contract,
                version: `${bootstrapFixture.contract.version}-unsupported`,
              },
            }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toBeInstanceOf(
      ProductContractError,
    )
  })

  it('rejects a bootstrap response beyond the fixed JSON byte bound', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify(bootstrapFixture), {
            headers: { 'content-length': '65537' },
          }),
      ),
    )

    const failure = await new SameOriginProductTransport()
      .readBootstrap()
      .catch((error: unknown) => error)
    expect(failure).toBeInstanceOf(ProductContractError)
    expect((failure as ProductContractError).cause).toMatchObject({
      message: expect.stringContaining('response exceeded the product JSON byte limit'),
    })
  })

  it('reports an unsuccessful HTTP response without decoding its body', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(null, { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readBootstrap()).rejects.toThrow('status 503')
  })

  it('resolves a blob descriptor through the generated contract decoder', async () => {
    const fetchRequest = vi.fn(async () => new Response(JSON.stringify(imageArtifact)))
    vi.stubGlobal('fetch', fetchRequest)

    const descriptor = await new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
      displayFilename: imageArtifact.display_filename[0],
    })

    expect(descriptor).toEqual(imageArtifact)
    expect(fetchRequest).toHaveBeenCalledWith(
      `/api/blobs/${encodeURIComponent(imageArtifact.digest)}/descriptor?media_type=image%2Fpng&display_filename=orbital-map.png`,
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('rejects a descriptor for a different immutable identity', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(fallbackDescriptor))),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })

    await expect(request).rejects.toThrow('descriptor digest did not match')
  })

  it('rejects descriptor metadata for a different semantic use', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...imageArtifact,
              declared_media_type: 'image/jpeg',
              display_filename: ['different.jpg'],
              available_views: [
                {
                  ...imageArtifact.available_views[0],
                  media_type: 'image/jpeg',
                  content_url: `/api/blobs/${imageArtifact.digest}/download?media_type=image%2Fjpeg&display_filename=different.jpg`,
                },
              ],
            }),
          ),
      ),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
      displayFilename: imageArtifact.display_filename[0],
    })

    await expect(request).rejects.toThrow('descriptor media type did not match')
  })

  it('rejects a mismatched descriptor filename', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...imageArtifact,
              display_filename: ['different.png'],
              available_views: imageArtifact.available_views.map((view) =>
                view.kind === 'download'
                  ? {
                      ...view,
                      content_url: `/api/blobs/${imageArtifact.digest}/download?media_type=image%2Fpng&display_filename=different.png`,
                    }
                  : view,
              ),
            }),
          ),
      ),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
      displayFilename: imageArtifact.display_filename[0],
    })

    await expect(request).rejects.toThrow('descriptor filename did not match')
  })

  it('rejects an unexpected descriptor filename when none was requested', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(imageArtifact))),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })

    await expect(request).rejects.toThrow('descriptor filename did not match')
  })

  it('bounds descriptor use metadata before constructing the request URL', async () => {
    const fetchRequest = vi.fn()
    vi.stubGlobal('fetch', fetchRequest)
    const transport = new SameOriginProductTransport()

    await expect(
      transport.readBlobDescriptor({
        digest: imageArtifact.digest,
        mediaType: 'x'.repeat(MAX_DECLARED_MEDIA_TYPE_BYTES + 1),
      }),
    ).rejects.toThrow('255-byte limit')
    await expect(
      transport.readBlobDescriptor({
        digest: imageArtifact.digest,
        mediaType: imageArtifact.declared_media_type,
        displayFilename: 'é'.repeat(MAX_DISPLAY_FILENAME_BYTES / 2 + 1),
      }),
    ).rejects.toThrow('1024-byte limit')
    expect(fetchRequest).not.toHaveBeenCalled()
  })

  it('accepts descriptor metadata at the exact UTF-8 ceilings', async () => {
    const mediaType = `application/${'a'.repeat(MAX_DECLARED_MEDIA_TYPE_BYTES - 'application/'.length)}`
    const displayFilename = 'é'.repeat(MAX_DISPLAY_FILENAME_BYTES / 2)
    const descriptor = {
      ...imageArtifact,
      declared_media_type: mediaType,
      display_filename: [displayFilename],
      available_views: [
        {
          ...imageArtifact.available_views[0],
          media_type: mediaType,
          content_url: `/api/blobs/${imageArtifact.digest}/download?${new URLSearchParams({
            media_type: mediaType,
            display_filename: displayFilename,
          }).toString()}`,
        },
      ],
    }
    const fetchRequest = vi.fn(async () => new Response(JSON.stringify(descriptor)))
    vi.stubGlobal('fetch', fetchRequest)

    await expect(
      new SameOriginProductTransport().readBlobDescriptor({
        digest: imageArtifact.digest,
        mediaType,
        displayFilename,
      }),
    ).resolves.toEqual(descriptor)
    expect(fetchRequest).toHaveBeenCalledOnce()
  })

  it('classifies descriptor limits as input failures', async () => {
    await expect(
      new SameOriginProductTransport().readBlobDescriptor({
        digest: imageArtifact.digest,
        mediaType: 'x'.repeat(MAX_DECLARED_MEDIA_TYPE_BYTES + 1),
      }),
    ).rejects.toBeInstanceOf(ProductInputError)
  })

  it('bounds descriptor error payloads before decoding them', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response('x'.repeat(MAX_PRODUCT_JSON_BYTES + 1), {
            status: 500,
            headers: { 'content-type': 'application/json' },
          }),
      ),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })
    await expect(request).rejects.toThrow('response exceeded the product JSON byte limit')
  })

  it('classifies an interrupted descriptor response stream as a transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => interruptedResponse()),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })

    await expect(request).rejects.toBeInstanceOf(ProductTransportError)
  })

  it('preserves a typed daemon error from descriptor resolution', async () => {
    const responseFixture = {
      error: {
        code: 'blob_not_found',
        kind: 'application',
        message: 'blob is not present',
      },
    } as const
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(responseFixture), { status: 404 })),
    )

    const request = new SameOriginProductTransport().readBlobDescriptor({
      digest: imageArtifact.digest,
      mediaType: imageArtifact.declared_media_type,
    })

    await expect(request).rejects.toEqual(
      expect.objectContaining({
        status: 404,
        response: responseFixture,
      }),
    )
  })

  it('decodes one bounded attention page and preserves its typed continuation', async () => {
    const summaries = Array.from({ length: MAX_ATTENTION_SNAPSHOT_ITEMS }, (_, index) => ({
      ...attentionFixture.summaries[0],
      session_id: `018f1840-6f3d-7a8b-9c1d-${(
        BigInt(`0x${sessionId.slice(-12)}`) + BigInt(index + 1)
      )
        .toString(16)
        .padStart(12, '0')}`,
    }))
    const pagedFixture = {
      ...attentionFixture,
      continuation_after_session_id: summaries.at(-1)?.session_id,
      summaries,
    }
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(pagedFixture)))
    vi.stubGlobal('fetch', fetchMock)

    const snapshot = await new SameOriginProductTransport().readAttention(sessionId)

    expect(snapshot).toEqual(pagedFixture)
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/attention?after_session_id=${sessionId}`,
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('preserves a typed attention projection failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(errorFixture), { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toEqual(
      new ProductRequestError(503, errorFixture),
    )
  })

  it('classifies a rejected attention fetch as a transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('Failed to fetch'))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toBeInstanceOf(
      ProductTransportError,
    )
  })

  it('rejects a typed error before buffering beyond the JSON byte ceiling', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(' '.repeat(MAX_PRODUCT_JSON_BYTES + 1), { status: 503 })),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'response exceeded the product JSON byte limit',
    )
  })

  it('decodes complete NDJSON attention events without buffering stream history', async () => {
    const body = `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n${JSON.stringify(attentionUpdateFixture)}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).resolves.toEqual({
      done: false,
      value: { kind: 'snapshot', snapshot: attentionFixture },
    })
    await expect(events.next()).resolves.toEqual({ done: false, value: attentionUpdateFixture })
    await expect(events.next()).resolves.toEqual({ done: true, value: undefined })
  })

  it('rejects a successful follow response with the wrong media type', async () => {
    const body = `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(body, { headers: { 'content-type': 'application/json' } })),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow(
      'attention stream response must use application/x-ndjson',
    )
  })

  it('rejects malformed cursors in HTTP snapshots and stream events', async () => {
    const malformedSnapshot = { ...attentionFixture, cursor: '01' }
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(new Response(JSON.stringify(malformedSnapshot)))
      .mockResolvedValueOnce(
        new Response(`${JSON.stringify({ ...attentionUpdateFixture, cursor: 'not-a-number' })}\n`, {
          headers: { 'content-type': 'application/x-ndjson' },
        }),
      )
    vi.stubGlobal('fetch', fetchMock)

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.cursor must be matching',
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()
    await expect(events.next()).rejects.toThrow('attention_event must be one recognized variant')
  })

  it('rejects an attention snapshot beyond the contract item ceiling', async () => {
    const summaries = Array.from({ length: MAX_ATTENTION_SNAPSHOT_ITEMS + 1 }, (_, index) => ({
      ...attentionFixture.summaries[0],
      session_id: `${sessionId}-${index}`,
    }))
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ ...attentionFixture, summaries }))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      `attention_snapshot.summaries must be at most ${MAX_ATTENTION_SNAPSHOT_ITEMS} items`,
    )
  })

  it('rejects duplicate session identities in HTTP attention snapshots', async () => {
    const duplicate = {
      ...attentionFixture,
      summaries: [attentionFixture.summaries[0], attentionFixture.summaries[0]],
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(duplicate))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention snapshot contains duplicate session identities',
    )
  })

  it('rejects duplicate session identities in streamed attention snapshots', async () => {
    const duplicate = {
      ...attentionFixture,
      summaries: [attentionFixture.summaries[0], attentionFixture.summaries[0]],
    }
    const body = `${JSON.stringify({ kind: 'snapshot', snapshot: duplicate })}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow(
      'attention snapshot contains duplicate session identities',
    )
  })

  it('rejects incoherent state and action pairs in snapshots and updates', async () => {
    const incoherentSummary = {
      ...attentionFixture.summaries[0],
      state: 'idle',
      action: 'restore_runner',
    }
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ ...attentionFixture, summaries: [incoherentSummary] })),
      )
      .mockResolvedValueOnce(
        new Response(
          `${JSON.stringify({ kind: 'update', cursor: '18', summaries: [incoherentSummary] })}\n`,
          { headers: { 'content-type': 'application/x-ndjson' } },
        ),
      )
    vi.stubGlobal('fetch', fetchMock)

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.summaries[0].action must be one recognized variant',
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()
    await expect(events.next()).rejects.toThrow('attention_event must be one recognized variant')
  })

  it('accepts an actionless approval wait', async () => {
    const actionless = { ...attentionFixture.summaries[0], action: null }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(JSON.stringify({ ...attentionFixture, summaries: [actionless] })),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual({
      ...attentionFixture,
      summaries: [actionless],
    })
  })

  it('accepts an actionless automatic-resumption block', async () => {
    const automaticResumptionBlock = {
      ...attentionFixture.summaries[0],
      action: null,
      goal_block: {
        generation: '3',
        need_summary: 'Retry execution after the automatic delay.',
        reason: 'execution_failure',
      },
      state: 'blocked',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({ ...attentionFixture, summaries: [automaticResumptionBlock] }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual({
      ...attentionFixture,
      summaries: [automaticResumptionBlock],
    })
  })

  it('accepts an actionless runner-loss summary', async () => {
    const runnerLost = {
      ...attentionFixture.summaries[0],
      action: null,
      state: 'runner_lost',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(JSON.stringify({ ...attentionFixture, summaries: [runnerLost] })),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual({
      ...attentionFixture,
      summaries: [runnerLost],
    })
  })

  it('accepts an actionless tool-recovery summary', async () => {
    const awaitingToolRecovery = {
      ...attentionFixture.summaries[0],
      action: null,
      state: 'awaiting_tool_recovery',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ ...attentionFixture, summaries: [awaitingToolRecovery] })),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual({
      ...attentionFixture,
      summaries: [awaitingToolRecovery],
    })
  })

  it('accepts an actionless reconciliation summary', async () => {
    const awaitingReconciliation = {
      ...attentionFixture.summaries[0],
      action: null,
      state: 'awaiting_reconciliation',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({ ...attentionFixture, summaries: [awaitingReconciliation] }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual({
      ...attentionFixture,
      summaries: [awaitingReconciliation],
    })
  })

  it('rejects turn-derived states without a current-turn identity', async () => {
    const withoutTurn = {
      ...attentionFixture.summaries[0],
      action: null,
      current_turn_id: null,
      state: 'active',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(JSON.stringify({ ...attentionFixture, summaries: [withoutTurn] })),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'turn-derived attention summary must include a current-turn identity',
    )
  })

  it('rejects a blocked summary without goal-block evidence', async () => {
    const blockedWithoutEvidence = {
      ...attentionFixture.summaries[0],
      action: 'provide_goal_need',
      goal_block: null,
      state: 'blocked',
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({ ...attentionFixture, summaries: [blockedWithoutEvidence] }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.summaries[0].goal_block must be consistent with attention state "blocked"',
    )
  })

  it('rejects a malformed session identity', async () => {
    const malformedIdentity = { ...attentionFixture.summaries[0], session_id: 'not-a-uuid' }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...attentionFixture,
              continuation_after_session_id: null,
              summaries: [malformedIdentity],
            }),
          ),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.summaries[0].session_id must be matching',
    )
  })

  it('rejects a malformed judge count', async () => {
    const malformedCount = {
      ...attentionFixture.summaries[0],
      judge: { ...attentionFixture.summaries[0].judge, failed: '-1' },
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ ...attentionFixture, summaries: [malformedCount] })),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.summaries[0].judge.failed must be matching',
    )
  })

  it('rejects streamed updates beyond the contract item ceiling', async () => {
    const summaries = Array.from({ length: MAX_ATTENTION_SNAPSHOT_ITEMS + 1 }, (_, index) => ({
      ...attentionFixture.summaries[0],
      session_id: `018f1840-6f3d-7a8b-9c1d-${index.toString(16).padStart(12, '0')}`,
    }))
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(`${JSON.stringify({ kind: 'update', cursor: '18', summaries })}\n`, {
            headers: { 'content-type': 'application/x-ndjson' },
          }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow('attention_event must be one recognized variant')
  })

  it('rejects attention summaries that are not ordered by session identity', async () => {
    const unordered = {
      ...attentionFixture,
      summaries: [
        { ...attentionFixture.summaries[0], session_id: laterSessionId },
        attentionFixture.summaries[0],
      ],
      continuation_after_session_id: sessionId,
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(unordered))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention snapshot summaries are not ordered by session identity',
    )
  })

  it('rejects a continuation that does not match the last session identity', async () => {
    const incoherent = { ...attentionFixture, continuation_after_session_id: laterSessionId }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(incoherent))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'attention_snapshot.continuation_after_session_id must be the last returned session identity',
    )
  })

  it('rejects paged summaries at or before the requested keyset cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(attentionFixture))),
    )

    await expect(new SameOriginProductTransport().readAttention(sessionId)).rejects.toThrow(
      'attention snapshot contains an identity at or before its keyset cursor',
    )
  })

  it('accepts an omitted optional continuation', async () => {
    const withoutContinuation = {
      cursor: attentionFixture.cursor,
      summaries: attentionFixture.summaries,
    }
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(withoutContinuation))),
    )

    await expect(new SameOriginProductTransport().readAttention()).resolves.toEqual(
      withoutContinuation,
    )
  })

  it('classifies an attention response-body read failure as a transport failure', async () => {
    const body = new ReadableStream<Uint8Array>({
      pull() {
        throw new TypeError('connection reset')
      },
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toBeInstanceOf(
      ProductTransportError,
    )
  })

  it('rejects an attention snapshot before buffering beyond the byte ceiling', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(' '.repeat(MAX_PRODUCT_JSON_BYTES + 1))),
    )

    await expect(new SameOriginProductTransport().readAttention()).rejects.toThrow(
      'response exceeded the product JSON byte limit',
    )
  })

  it('rejects an attention event beyond the advertised NDJSON item ceiling', async () => {
    const body = `${' '.repeat(bootstrapFixture.limits.max_ndjson_item_bytes + 1)}\n`
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow(
      'attention stream item exceeds the contract ceiling',
    )
  })

  it('rejects a final attention event without its record delimiter', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify(attentionUpdateFixture), {
            headers: { 'content-type': 'application/x-ndjson' },
          }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).rejects.toThrow('attention stream ended with an incomplete item')
  })

  it('cancels a follower stream when its consumer reconnects early', async () => {
    let cancelled = false
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(
          new TextEncoder().encode(
            `${JSON.stringify({ kind: 'snapshot', snapshot: attentionFixture })}\n`,
          ),
        )
      },
      cancel() {
        cancelled = true
      },
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () => new Response(body, { headers: { 'content-type': 'application/x-ndjson' } }),
      ),
    )
    const events = new SameOriginProductTransport().followAttention()[Symbol.asyncIterator]()

    await expect(events.next()).resolves.toEqual({
      done: false,
      value: { kind: 'snapshot', snapshot: attentionFixture },
    })
    await events.return?.(undefined)

    expect(cancelled).toBe(true)
  })

  const rejectBootstrapLimits = async (limits: Record<string, number>) => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...bootstrapFixture,
              limits: { ...bootstrapFixture.limits, ...limits },
            }),
          ),
      ),
    )
    const failure = await new SameOriginProductTransport()
      .readBootstrap()
      .catch((error: unknown) => error)
    expect(failure).toBeInstanceOf(ProductContractError)
    expect((failure as ProductContractError).cause).toMatchObject({
      message: expect.stringContaining('invalid search'),
    })
  }

  it('rejects a zero search-query ceiling', () =>
    rejectBootstrapLimits({ max_search_query_bytes: 0 }))

  it('rejects an excessive search-query ceiling', () =>
    rejectBootstrapLimits({ max_search_query_bytes: 513 }))

  it('rejects a zero search-page ceiling', () =>
    rejectBootstrapLimits({ max_search_page_items: 0 }))

  it('rejects an excessive search-page ceiling', () =>
    rejectBootstrapLimits({ max_search_page_items: 101 }))

  it('rejects a zero search-snippet ceiling', () =>
    rejectBootstrapLimits({ max_search_snippet_bytes: 0 }))

  it('rejects an excessive search-snippet ceiling', () =>
    rejectBootstrapLimits({ max_search_snippet_bytes: 513 }))

  it('decodes a bounded search page and sends product vocabulary', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(searchPageFixture)))
    vi.stubGlobal('fetch', fetchMock)
    const request = {
      query: 'natural terms',
      sessionId: searchPageFixture.results[0].session_id,
      maxItems: 100,
      maxSnippetBytes: 512,
      after: { address: '1000', projectionId: '42' },
    }
    const expectedSearch = new URLSearchParams({
      strategy: 'lexical',
      q: request.query,
      max_items: String(request.maxItems),
      session_id: request.sessionId,
      after_address: request.after.address,
      after_projection: request.after.projectionId,
    })

    const page = await new SameOriginProductTransport().search(request)

    expect(page).toEqual(searchPageFixture)
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/search?${expectedSearch.toString()}`,
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('preserves a typed application search failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(searchErrorFixture), { status: 503 })),
    )

    const pending = new SameOriginProductTransport().search({
      query: 'term',
      maxItems: 10,
      maxSnippetBytes: 512,
    })

    await expect(pending).rejects.toEqual(new ProductRequestError(503, searchErrorFixture))
  })

  it('distinguishes an unreachable search transport from contract decoding failures', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('Failed to fetch'))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 10,
        maxSnippetBytes: 512,
      }),
    ).rejects.toBeInstanceOf(ProductTransportError)
  })

  it('preserves transport identity when a search response stream is interrupted', async () => {
    const body = new ReadableStream<Uint8Array>({
      pull(controller) {
        controller.error(new TypeError('connection reset'))
      },
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(body)),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toBeInstanceOf(ProductTransportError)
  })

  it('rejects an encoded search response beyond its byte bound', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify(searchPageFixture), {
            headers: { 'content-length': '9999999' },
          }),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('response exceeded')
  })

  it('accepts bounded snippets at their worst-case JSON expansion', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(escapedSearchPageFixture))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 100,
        maxSnippetBytes: 512,
      }),
    ).resolves.toEqual(escapedSearchPageFixture)
  })

  it('rejects decoded search fields beyond their rendering bounds', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [{ ...searchPageFixture.results[0], snippet: 'too long' }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 3,
      }),
    ).rejects.toThrow('snippet limit')
  })
  it('rejects results outside an exact-session request', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                {
                  ...searchPageFixture.results[0],
                  session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e',
                  source: {
                    kind: 'session',
                    session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e',
                  },
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        sessionId: searchPageFixture.results[0].session_id,
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('outside the requested session')
  })

  it('canonicalizes exact-session UUID spellings before comparing results', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(searchPageFixture))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        sessionId: `URN:UUID:{${searchPageFixture.results[0].session_id.toUpperCase()}}`,
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).resolves.toEqual(searchPageFixture)
  })

  it('rejects session source identities that contradict the result session', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                {
                  ...searchPageFixture.results[0],
                  source: {
                    kind: 'session',
                    session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c7e',
                  },
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('source consistent with the result session and content class')
  })

  it('rejects a malformed result session UUID', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [{ ...searchPageFixture.results[0], session_id: 'not-a-uuid' }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('session_id must be matching')
  })

  it('rejects a malformed typed source UUID', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                {
                  ...searchPageFixture.results[0],
                  source: { kind: 'session', session_id: 'not-a-uuid' },
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('source must be one recognized variant')
  })

  it('rejects highlight offsets inside a UTF-8 character', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                {
                  ...searchPageFixture.results[0],
                  snippet: 'évidence',
                  highlights: [{ start_byte: 1, end_byte: 2 }],
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('UTF-8 boundaries')
  })

  it('rejects more highlights than the generated contract permits', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                {
                  ...searchPageFixture.results[0],
                  snippet: 'x'.repeat(130),
                  highlights: Array.from({ length: 65 }, (_, index) => ({
                    start_byte: index * 2,
                    end_byte: index * 2 + 1,
                  })),
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('at most 64 items')
  })

  it('rejects search pages that are not ordered newest first', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                { ...searchPageFixture.results[0], address: { event_sequence: '750' } },
                { ...searchPageFixture.results[0], address: { event_sequence: '901' } },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 2,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('strictly descending search result key')
  })

  it('rejects same-address results whose projection IDs are not newest first', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                { ...searchPageFixture.results[0], projection_id: '41' },
                { ...searchPageFixture.results[0], projection_id: '42' },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 2,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('strictly descending search result key')
  })

  it('rejects a result address above u64', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                {
                  ...searchPageFixture.results[0],
                  address: { event_sequence: '18446744073709551616' },
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('unsigned 64-bit integer')
  })

  it('rejects a result projection ID above positive i64', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...searchPageFixture,
              results: [
                {
                  ...searchPageFixture.results[0],
                  projection_id: '9223372036854775808',
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('positive signed 64-bit integer')
  })

  it('rejects a page that repeats the request cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(searchPageFixture))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
        after: {
          address: searchPageFixture.results[0].address.event_sequence,
          projectionId: searchPageFixture.results[0].projection_id,
        },
      }),
    ).rejects.toThrow('does not advance past the request cursor')
  })

  it('rejects a page newer than the request cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify(searchPageFixture))),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
        after: { address: '900', projectionId: '42' },
      }),
    ).rejects.toThrow('does not advance past the request cursor')
  })

  const rejectContinuation = async (
    continuation: {
      address: { event_sequence: string }
      projection_id: string
    },
    expectedError = 'invalid continuation',
  ) => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ ...searchPageFixture, continuation }))),
    )
    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow(expectedError)
  }

  it('rejects a continuation address detached from the returned page', () =>
    rejectContinuation(
      { address: { event_sequence: '900' }, projection_id: '42' },
      'last result ordering key',
    ))

  it('rejects a continuation projection detached from the returned page', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: '41' },
      'last result ordering key',
    ))

  it('rejects a nondecimal continuation projection ID', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: 'not-decimal' },
      'recognized variant',
    ))

  it('rejects a zero continuation projection ID', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: '0' },
      'recognized variant',
    ))

  it('rejects a continuation projection ID above positive i64', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: '9223372036854775808' },
      'recognized variant',
    ))

  it('rejects a continuation projection ID above u64', () =>
    rejectContinuation(
      { address: { event_sequence: '901' }, projection_id: '18446744073709551616' },
      'recognized variant',
    ))

  it('rejects a continuation on an empty page', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              results: [],
              continuation: { address: { event_sequence: '901' }, projection_id: '42' },
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().search({
        query: 'term',
        maxItems: 1,
        maxSnippetBytes: 512,
      }),
    ).rejects.toThrow('last result ordering key')
  })
})

describe('readProductSearchState', () => {
  it('keeps only nonempty typed URL fields', () => {
    expect(
      readProductSearchState({ q: 'term', session: '', afterAddress: 7, around: '901' }),
    ).toEqual({
      q: 'term',
      session: undefined,
      afterAddress: '7',
      afterProjection: undefined,
      around: '901',
    })
  })

  it('preserves JSON-like numeric and boolean query text', () => {
    expect(readProductSearchState({ q: 2026 }).q).toBe('2026')
    expect(readProductSearchState({ q: true }).q).toBe('true')
    expect(readProductSearchState({ q: null }).q).toBe('null')
  })

  it('preserves numeric-looking cursor fields for pair validation', () => {
    expect(readProductSearchState({ q: 'term', afterAddress: 750 })).toEqual({
      q: 'term',
      session: undefined,
      afterAddress: '750',
      afterProjection: undefined,
      around: undefined,
    })
  })

  it('preserves repeated exact-session parameters as invalid state', () => {
    expect(readProductSearchState({ q: 'term', session: ['first', 'second'] })).toEqual({
      q: 'term',
      session: undefined,
      sessionParameterIsValid: false,
      afterAddress: undefined,
      afterProjection: undefined,
      around: undefined,
    })
  })

  it('preserves a non-string exact-session parameter as invalid state', () => {
    expect(readProductSearchState({ q: 'term', session: 123 })).toEqual({
      q: 'term',
      session: undefined,
      sessionParameterIsValid: false,
      afterAddress: undefined,
      afterProjection: undefined,
      around: undefined,
    })
  })

  it('preserves repeated cursor parameters as invalid state', () => {
    expect(
      readProductSearchState({
        q: 'term',
        afterAddress: ['750', '700'],
        afterProjection: ['42', '41'],
      }),
    ).toEqual({
      q: 'term',
      session: undefined,
      afterAddress: undefined,
      afterProjection: undefined,
      cursorParametersAreValid: false,
      around: undefined,
    })
  })
})

describe('readProductRouteState', () => {
  it('retains catalog continuation fields beside search route state', () => {
    expect(
      readProductRouteState({
        afterSession: previousSessionId,
        afterActivity: 1_724_194_799_998_971,
      }),
    ).toMatchObject({
      afterSession: previousSessionId,
      afterActivity: '1724194799998971',
    })
  })
})

describe('product surface availability', () => {
  it('requires bounded JSON before enabling timeline reads', () => {
    expect(
      hasValidSessionTimelineContract({
        ...webContractBootstrapFixture,
        capabilities: { ...webContractBootstrapFixture.capabilities, bounded_json: false },
      }),
    ).toBe(false)
  })

  it('rejects timeline capability with unusable semantic limits', () => {
    expect(hasValidSessionTimelineContract(webContractBootstrapFixture)).toBe(true)
    expect(
      hasValidSessionTimelineContract({
        ...webContractBootstrapFixture,
        limits: { ...webContractBootstrapFixture.limits, max_timeline_window_items: 0 },
      }),
    ).toBe(false)
    expect(
      hasValidSessionTimelineContract({
        ...webContractBootstrapFixture,
        limits: { ...webContractBootstrapFixture.limits, max_timeline_window_bytes: 65_537 },
      }),
    ).toBe(false)
  })

  it('defines one typed authority state for every product route', () => {
    expect(productSurfaceStates).toHaveProperty(productRoutes[0].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[1].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[2].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[3].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[4].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[5].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[6].id)
    expect(productSurfaceStates).toHaveProperty(productRoutes[7].id)
  })

  it('keeps Settings browser-local instead of implying daemon authority', () => {
    expect(productSurfaceStates.settings).toEqual({
      kind: 'browser-local',
      authority: 'browser preferences',
    })
  })

  it('marks the available Session timeline facts as server-backed', () => {
    expect(productSurfaceStates.sessions).toEqual({
      kind: 'server-backed',
      owningTrack: '#991 session projections',
      facts: ['bounded session descriptors', 'stable-address timeline windows'],
    })
  })

  it('marks the mounted Attention reads as server-backed', () => {
    expect(productSurfaceStates.attention).toEqual({
      kind: 'server-backed',
      owningTrack: '#992 attention projections',
      facts: ['keyset attention snapshot pages', 'streamed attention projection updates'],
    })
  })

  it('marks the mounted Imports reads as server-backed', () => {
    expect(productSurfaceStates.imports).toEqual({
      kind: 'server-backed',
      owningTrack: '#995 discovery reads',
      facts: ['keyset import catalog pages', 'bounded imported-entry windows'],
    })
  })

  it('reports cache ownership only for implemented surfaces', () => {
    expect(productSurfaceCacheLabel('attention')).toBe('Bounded query')
    expect(productSurfaceCacheLabel('sessions')).toBe('Bounded query')
    expect(productSurfaceCacheLabel('imports')).toBe('Bounded query')
    expect(productSurfaceCacheLabel('settings')).toBe('Local settings')
    expect(productSurfaceCacheLabel('search')).toBeNull()
  })
})
