import { describe, expect, it } from 'vitest'
import {
  SCENARIO_ENTRY_TOTAL,
  SCENARIO_IMPORT_LIST_ITEMS,
  SCENARIO_IMPORT_TOTAL,
  SCENARIO_IMPORT_WINDOW_ITEMS,
  ScenarioImportApi,
} from './scenario'

const firstImportId = '00000000-0000-7000-8000-000000000001'

describe('ScenarioImportApi', () => {
  it('bounds a million-row imports catalog with a stable keyset cursor', async () => {
    const api = new ScenarioImportApi()
    const page = await api.list({ limit: SCENARIO_IMPORT_TOTAL })

    expect(page.items).toHaveLength(SCENARIO_IMPORT_LIST_ITEMS)
    expect(page.next_cursor).toBe(page.items.at(-1)?.imported_conversation_id)
  })

  it('keeps adjacent keyset pages stable when a later import arrives', async () => {
    const api = new ScenarioImportApi()
    const first = await api.list({ limit: 2 })
    api.injectConcurrentImport()
    const second = await api.list({ after: first.next_cursor, limit: 2 })

    expect(first.items.map((item) => item.imported_conversation_id)).toEqual([
      '00000000-0000-7000-8000-000000000001',
      '00000000-0000-7000-8000-000000000002',
    ])
    expect(second.items.map((item) => item.imported_conversation_id)).toEqual([
      '00000000-0000-7000-8000-000000000003',
      '00000000-0000-7000-8000-000000000004',
    ])
  })

  it('returns only one bounded region around an arbitrary enormous-import position', async () => {
    const api = new ScenarioImportApi()
    const window = await api.entries(firstImportId, {
      anchor: 'position',
      position: 125_000,
      before: 50,
      after: 50,
    })

    expect(window.items).toHaveLength(SCENARIO_IMPORT_WINDOW_ITEMS)
    expect(window.anchor_position).toBe(125_000)
    expect(window.has_before).toBe(true)
    expect(window.has_after).toBe(true)
  })

  it('describes source evidence and immutable bounds without materializing entries', async () => {
    const api = new ScenarioImportApi()
    const descriptor = await api.descriptor(firstImportId)

    expect(descriptor.entry_count).toBe(SCENARIO_ENTRY_TOTAL)
    expect(descriptor.timeline.first.position).toBe(1)
    expect(descriptor.timeline.latest.position).toBe(descriptor.entry_count)
    expect(descriptor.source.source_session_id?.leading_text).toBe('source-session-0')
  })

  it('treats a null source-session filter as no filter', async () => {
    const api = new ScenarioImportApi()
    const page = await api.list({ source_session_id: null, limit: 2 })

    expect(page.items).toHaveLength(2)
  })

  it('replays one command identity while assigning distinct sessions to distinct commands', async () => {
    const api = new ScenarioImportApi()
    const frontier = {
      imported_conversation_id: firstImportId,
      imported_entry_id: '00000000-0000-7000-8000-000000000002',
      position: 1,
    }
    const firstRequest = {
      command_id: '00000000-0000-7000-8000-000000000003',
      frontier,
      relationship: 'resume' as const,
      initial_model_selection: {
        kind: 'direct' as const,
        selection_id: '00000000-0000-7000-8000-000000000004',
      },
    }
    const secondRequest = {
      ...firstRequest,
      command_id: '00000000-0000-7000-8000-000000000005',
      relationship: 'fork' as const,
    }

    const first = await api.continueImport(firstImportId, firstRequest)
    const replay = await api.continueImport(firstImportId, firstRequest)
    const second = await api.continueImport(firstImportId, secondRequest)

    expect(replay.session_id).toBe(first.session_id)
    expect(second.session_id).not.toBe(first.session_id)
  })
})
