import { describe, expect, it } from 'vitest'
import {
  detailExcerpt,
  detailItems,
  detailPage,
  resultCursor,
  toolResultItem,
} from '../../e2e/session-detail-fixture'
import type { WebSessionTimelineDetail } from '../generated/web-contract.mjs'
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
