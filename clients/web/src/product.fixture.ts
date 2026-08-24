// Keep browser tests aligned with WebContractBootstrap::current() and its Rust-authored limits.
export const webContractBootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '1' },
  capabilities: {
    bounded_json: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
    bounded_session_timeline: true,
    bounded_session_timeline_detail: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 65_536,
    max_timeline_window_items: 256,
    max_timeline_window_bytes: 65_536,
    max_timeline_detail_items: 128,
    max_timeline_detail_bytes: 65_536,
  },
} as const
