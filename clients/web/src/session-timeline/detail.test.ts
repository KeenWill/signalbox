import { describe, expect, it } from 'vitest'
import {
  detailExcerpt,
  detailItems,
  detailPage,
  resultCursor,
  toolResultItem,
} from '../../e2e/session-detail-fixture'
import {
  decodeWebSessionTimelineDetailPage,
  type WebSessionTimelineDetail,
} from '../generated/web-contract.mjs'
import { validateDetailContinuation } from './model'

const argumentsItem = () => {
  const item = structuredClone(detailItems[1])
  if (!item || item.body.type !== 'tool_batch') throw new Error('tool arguments fixture missing')
  return item as WebSessionTimelineDetail & {
    body: Extract<WebSessionTimelineDetail['body'], { type: 'tool_batch' }>
  }
}

describe('typed detail continuation', () => {
  it('accepts the exact member and field requested by the prior page', () => {
    const previous = detailPage([argumentsItem()], resultCursor)
    expect(() => validateDetailContinuation(previous, null)).not.toThrow()
    expect(() =>
      validateDetailContinuation(detailPage([toolResultItem()]), resultCursor, previous),
    ).not.toThrow()
  })

  it('rejects a tool page that drops its advertised terminal payload', () => {
    expect(() => validateDetailContinuation(detailPage([argumentsItem()]), null)).toThrow(
      'terminal payload continuation',
    )
    const skipResult = {
      ...resultCursor,
      body: { ...resultCursor.body, field: 'tool_arguments' as const, member_index: 1 },
    }
    expect(() =>
      validateDetailContinuation(detailPage([argumentsItem()], skipResult), null),
    ).toThrow('terminal payload continuation')
  })

  it('rejects a repeated member returned for a different member cursor', () => {
    const cursor = { ...resultCursor, body: { ...resultCursor.body, member_index: 1 } }
    expect(() => validateDetailContinuation(detailPage([toolResultItem()]), cursor)).toThrow(
      'body continuation',
    )
  })

  it('rejects a changed request identity within the same member', () => {
    const previousItem = argumentsItem()
    const tool = previousItem.body.tools[0]
    if (!tool) throw new Error('tool member missing')
    const changed = {
      ...previousItem,
      body: {
        ...previousItem.body,
        tools: [{ ...tool, request_id: '00000000-0000-0000-0000-000000000126' }],
      },
    }
    expect(() =>
      validateDetailContinuation(
        detailPage([toolResultItem()]),
        resultCursor,
        detailPage([changed], resultCursor),
      ),
    ).toThrow('tool member identity')
  })

  it.each([
    { attempt_id: '00000000-0000-0000-0000-000000000126' },
    {
      state: 'known_failed',
      result_present: false,
      failure_present: true,
      cause: 'execution_failed',
    },
    { effect_posture: 'external_effect' },
    { sandbox_posture: 'unsandboxed' },
  ] as const)('rejects changed frozen attempt evidence across argument chunks: %j', (changed) => {
    const item = argumentsItem()
    const tool = item.body.tools[0]
    if (!tool || tool.evidence.type !== 'physical_attempt')
      throw new Error('physical attempt fixture missing')
    const cursor = {
      ...resultCursor,
      body: { ...resultCursor.body, field: 'tool_arguments' as const, offset_bytes: '1' },
    }
    const nextCursor = { ...cursor, body: { ...cursor.body, offset_bytes: '2' } }
    const chunk = (
      text: string,
      offset: string,
      next: typeof cursor,
      evidence: typeof tool.evidence,
    ) =>
      decodeWebSessionTimelineDetailPage(
        detailPage(
          [
            {
              ...item,
              projected_body_bytes: 129,
              body: {
                ...item.body,
                tools: [
                  {
                    ...tool,
                    evidence,
                    arguments: {
                      text,
                      offset_bytes: offset,
                      total_bytes: '3',
                      continuation: next.body,
                    },
                  },
                ],
              },
            },
          ],
          next,
        ),
      )
    const previous = chunk('a', '0', cursor, tool.evidence)
    const continued = chunk('b', '1', nextCursor, { ...tool.evidence, ...changed })
    expect(() => validateDetailContinuation(previous, null)).not.toThrow()
    expect(() => validateDetailContinuation(continued, cursor, previous)).toThrow(
      'immutable tool attempt evidence',
    )
  })

  it('rejects a continued field that changes its immutable total length', () => {
    const item = toolResultItem()
    if (item.body.type !== 'tool_batch') throw new Error('tool result missing')
    const tool = item.body.tools[0]
    if (!tool || tool.evidence.type !== 'physical_attempt')
      throw new Error('physical result missing')
    const changed = {
      ...item,
      body: {
        ...item.body,
        tools: [
          {
            ...tool,
            evidence: {
              ...tool.evidence,
              result: { ...detailExcerpt('changed'), total_bytes: '999' },
            },
          },
        ],
      },
    }
    expect(() =>
      validateDetailContinuation(detailPage([changed]), resultCursor, detailPage([item])),
    ).toThrow('total byte length')
  })

  it('starts fresh reads at the first member and arguments field', () => {
    expect(() => validateDetailContinuation(detailPage([toolResultItem()]), null)).toThrow(
      'skips a text field',
    )
    const item = argumentsItem()
    const changed = { ...item, body: { ...item.body, projected_member_index: 1 } }
    expect(() => validateDetailContinuation(detailPage([changed]), null)).toThrow('skips a member')
  })
})
