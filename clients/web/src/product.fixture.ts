// Keep browser tests aligned with WebContractBootstrap::current() and its Rust-authored limits.
export const webContractBootstrapFixture = {
  contract: { name: 'signalbox.web-http', version: '2' },
  capabilities: {
    bounded_json: true,
    same_origin_json_mutations: true,
    ndjson_streaming: true,
    import_discovery: true,
    imported_continuations: true,
    bounded_session_timeline: true,
  },
  limits: {
    max_json_body_bytes: 65_536,
    max_ndjson_item_bytes: 65_536,
    max_timeline_window_items: 256,
    max_timeline_window_bytes: 65_536,
  },
} as const
