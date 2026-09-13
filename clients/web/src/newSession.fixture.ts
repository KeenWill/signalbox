import type { WebCreateSessionResponse } from './generated/web-contract.mjs'

// Synthetic creation receipt shared by API and browser tests.
export const createdSessionFixture: WebCreateSessionResponse = {
  session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d',
  summary: {
    action: null,
    active_turn_count: '0',
    archived: false,
    current_turn_id: null,
    goal_block: null,
    judge: { actionable: '0', completed: '0', escalated: '0', failed: '0' },
    last_activity: { kind: 'session', unix_microseconds: '1724200000000000' },
    queued_turn_count: '0',
    repository_watch: null,
    session_id: '018f1840-6f3d-7a8b-9c1d-0e2f3a4b5c6d',
    state: 'idle',
    title_summary: 'New conversation',
    title_truncated: false,
  },
}
