import { spawnSync } from 'node:child_process'
import { cpSync } from 'node:fs'
import { join, resolve } from 'node:path'

const updateSnapshots = process.argv
  .slice(2)
  .some(
    (argument) => argument === '--update-snapshots' || argument.startsWith('--update-snapshots='),
  )

const result = spawnSync(
  'bazel',
  [
    'test',
    '//clients/web:browser_tests',
    '--test_output=errors',
    ...(updateSnapshots ? ['--nocache_test_results', '--nozip_undeclared_test_outputs'] : []),
    ...process.argv.slice(2).map((argument) => `--test_arg=${argument}`),
  ],
  { stdio: 'inherit' },
)
if (result.error) throw result.error
if (result.signal) throw new Error(`Browser tests exited on ${result.signal}`)
if (result.status === 0 && updateSnapshots) {
  const info = spawnSync('bazel', ['info', 'bazel-testlogs'], { encoding: 'utf8' })
  if (info.error) throw info.error
  if (info.status !== 0) throw new Error(info.stderr)
  cpSync(
    join(info.stdout.trim(), 'clients/web/browser_tests/test.outputs/updated-snapshots/e2e'),
    resolve('e2e'),
    { recursive: true },
  )
}
process.exit(result.status ?? 1)
