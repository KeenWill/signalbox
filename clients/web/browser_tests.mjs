import { spawnSync } from 'node:child_process'

const result = spawnSync(
  'bazel',
  [
    'test',
    '//clients/web:browser_tests',
    '--test_output=errors',
    ...process.argv.slice(2).map((argument) => `--test_arg=${argument}`),
  ],
  { stdio: 'inherit' },
)
if (result.error) throw result.error
if (result.signal) throw new Error(`Browser tests exited on ${result.signal}`)
process.exit(result.status ?? 1)
