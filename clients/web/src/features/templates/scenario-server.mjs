import { createServer } from 'vite'

// Deterministic browser scenarios do not need file watching on the shared test host.
const server = await createServer({ server: { host: '127.0.0.1', port: 4174, watch: null } })
await server.listen()
