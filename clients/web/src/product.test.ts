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

const activityFixture = {
  event_continuation_before: { cursor_generation: '8', event_ordinal: 41 },
  events: [
    {
      cursor_generation: '8',
      event_ordinal: 41,
      id: 'event-41',
      kind: 'head_changed',
      observed_at_unix_milliseconds: '1724200000000',
      pull_request: null,
    },
  ],
  webhook_continuation_before_receipt_sequence: null,
  webhooks: [],
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

  it('encodes both fields of an event cursor and an independently exhausted webhook feed', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(activityFixture)))
    vi.stubGlobal('fetch', fetchMock)

    const page = await new SameOriginProductTransport().readRepoWatchActivity(
      'example/repository',
      {
        eventBefore: { cursorGeneration: '9', eventOrdinal: 42 },
        includeEvents: true,
        includeWebhooks: false,
      },
    )

    expect(page).toEqual(activityFixture)
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/repository-watch/activity?repository=example%2Frepository&include_events=true&include_webhooks=false&event_before_cursor_generation=9&event_before_ordinal=42',
      expect.objectContaining({ credentials: 'same-origin' }),
    )
  })

  it('rejects non-advancing activity continuations', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              event_continuation_before: { cursor_generation: '9', event_ordinal: 42 },
              webhook_continuation_before_receipt_sequence: '7',
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository', {
        eventBefore: { cursorGeneration: '9', eventOrdinal: 42 },
        webhookBeforeReceiptSequence: '7',
        includeEvents: true,
        includeWebhooks: true,
      }),
    ).rejects.toThrow('does not advance to older history')
  })

  it('rejects activity rows outside the requested older window', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              event_continuation_before: null,
              events: [
                {
                  cursor_generation: '9',
                  event_ordinal: 42,
                  id: 'event-42',
                  kind: 'head_changed',
                  observed_at_unix_milliseconds: '1724200000000',
                  pull_request: null,
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository', {
        eventBefore: { cursorGeneration: '9', eventOrdinal: 42 },
        includeEvents: true,
        includeWebhooks: false,
      }),
    ).rejects.toThrow('event rows do not advance to older history')
  })

  it('rejects an activity continuation that does not equal the final row', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              event_continuation_before: { cursor_generation: '8', event_ordinal: 40 },
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('event continuation does not equal the returned page boundary')
  })

  it('rejects duplicate event identities in one activity page', async () => {
    const event = {
      cursor_generation: '9',
      event_ordinal: 42,
      id: 'event-42',
      kind: 'head_changed',
      observed_at_unix_milliseconds: '1724200000000',
      pull_request: null,
    } as const
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              event_continuation_before: null,
              events: [event, { ...event, cursor_generation: '8', event_ordinal: 41 }],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('activity page repeats an event identity')
  })

  it('fails closed when a repository-watch response carries an unknown field', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(JSON.stringify({ ...activityFixture, invented: true }))),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('webrepowatchactivitypage.invented must be absent')
  })

  it('rejects an oversized JSON response before parsing it', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response(' '.repeat(bootstrapFixture.limits.max_json_body_bytes + 1))),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('response exceeded the product JSON byte limit')
  })

  it('rejects a pull-request page for a different repository', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              repository: 'outside/repository',
              pull_requests: [],
              continuation_after_pull_request: null,
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchPullRequests('example/repository'),
    ).rejects.toThrow('does not match the requested repository')
  })

  it('rejects a non-advancing repository continuation', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              continuation_after_repository: 'example/repository',
              repositories: [],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchRepositories('example/repository'),
    ).rejects.toThrow('repository continuation does not equal the returned page boundary')
  })

  it('rejects a repeated held-work cursor', async () => {
    const dispatchId = '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d'
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              held_continuation_after: {
                dispatch_id: dispatchId,
                held_since_unix_microseconds: '1724200000000000',
              },
              held_slots: [],
              obligation_continuation_after: null,
              queued_obligations: [],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchWork('example/repository', {
        dispatchId,
        heldSinceUnixMicroseconds: '1724200000000000',
      }),
    ).rejects.toThrow('held-work continuation does not equal the returned page boundary')
  })

  it('rejects sessions that do not advance to older history', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              continuation_before: null,
              sessions: [
                {
                  attention: attentionFixture.summaries[0],
                  commissioned_at_unix_microseconds: '1724200000000000',
                  purpose: {
                    dispatch_id: 'dispatch-1',
                    kind: 'operator_commission',
                    template: 'review',
                  },
                },
              ],
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchPullRequestSessions(
        'example/repository',
        '17',
        { commissionedAtUnixMicroseconds: '1724200000000000', sessionId: previousSessionId },
      ),
    ).rejects.toThrow('session page does not advance to older history')
  })

  it('rejects a non-advancing pull-request continuation', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              repository: 'example/repository',
              pull_requests: [],
              continuation_after_pull_request: '64',
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchPullRequests('example/repository', '64'),
    ).rejects.toThrow('pull-request continuation does not equal the returned page boundary')
  })

  it('accepts the maximum unsigned pull-request cursor', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              repository: 'example/repository',
              pull_requests: [],
              continuation_after_pull_request: null,
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchPullRequests(
        'example/repository',
        '18446744073709551615',
      ),
    ).resolves.toEqual({
      repository: 'example/repository',
      pull_requests: [],
      continuation_after_pull_request: null,
    })
  })

  it('rejects an activity feed beyond its generated page ceiling', async () => {
    const webhook = {
      action_name: 'opened',
      disposition: 'projected',
      event_name: 'pull_request',
      latest_projected_at_unix_milliseconds: '1724200000000',
      projection_count: '1',
      receipt_sequence: '1',
      received_at_unix_milliseconds: '1724200000000',
    } as const
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              ...activityFixture,
              webhooks: Array.from({ length: 101 }, (_, index) => ({
                ...webhook,
                receipt_sequence: String(index + 1),
              })),
            }),
          ),
      ),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toThrow('at most 100 items')
  })

  it('classifies a rejected repository-watch fetch as a transport failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => Promise.reject(new TypeError('offline'))),
    )

    await expect(
      new SameOriginProductTransport().readRepoWatchActivity('example/repository'),
    ).rejects.toBeInstanceOf(ProductTransportError)
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
    expect(productSurfaceStates).toHaveProperty(productRoutes[8].id)
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
