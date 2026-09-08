import { spawnSync } from 'node:child_process'
import {
  chmodSync,
  closeSync,
  copyFileSync,
  linkSync,
  mkdirSync,
  openSync,
  readdirSync,
  readSync,
  realpathSync,
  statSync,
  symlinkSync,
} from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const runfiles = join(process.env.TEST_SRCDIR, process.env.TEST_WORKSPACE)
const runtime = dirname(resolve(runfiles, process.env.PLAYWRIGHT_RUNTIME_ROOT))
const patchelf = resolve(runfiles, process.env.PLAYWRIGHT_PATCHELF)
const temporary = process.env.TEST_TMPDIR
const libraries = join(runtime, 'usr/lib/x86_64-linux-gnu')
const loader = join(libraries, 'ld-linux-x86-64.so.2')
const browsers = join(temporary, 'ms-playwright')
const copiedFiles = new Map()
function materialize(source, destination) {
  if (statSync(source).isDirectory()) {
    mkdirSync(destination, { recursive: true })
    for (const entry of readdirSync(source)) {
      materialize(join(source, entry), join(destination, entry))
    }
  } else {
    const original = realpathSync(source)
    const copied = copiedFiles.get(original)
    if (copied) linkSync(copied, destination)
    else {
      copyFileSync(source, destination)
      copiedFiles.set(original, destination)
    }
  }
}
materialize(join(runtime, 'ms-playwright'), browsers)
const node = join(temporary, 'node')
copyFileSync(process.execPath, node)

function patchExecutable(path) {
  const descriptor = openSync(path, 'r')
  const header = Buffer.alloc(4)
  try {
    readSync(descriptor, header)
  } finally {
    closeSync(descriptor)
  }
  if (!header.equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46]))) return
  const interpreter = spawnSync(patchelf, ['--print-interpreter', path], {
    encoding: 'utf8',
  })
  if (interpreter.status !== 0) return // Shared libraries have no ELF interpreter.
  const patched = spawnSync(patchelf, ['--set-interpreter', loader, path], {
    encoding: 'utf8',
  })
  if (patched.status !== 0) throw new Error(patched.stderr)
}

for (const entry of readdirSync(browsers, {
  recursive: true,
  withFileTypes: true,
})) {
  if (entry.isFile()) patchExecutable(join(entry.parentPath, entry.name))
}
patchExecutable(node)
const project = join(temporary, 'web')
mkdirSync(project)
for (const entry of readdirSync(process.cwd())) {
  if (entry === 'node_modules') continue
  materialize(entry, join(project, entry))
}
symlinkSync(realpathSync('node_modules'), join(project, 'node_modules'))
const evidence = process.env.TEST_UNDECLARED_OUTPUTS_DIR
mkdirSync(evidence, { recursive: true })
const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`
const vite = join(dirname(fileURLToPath(import.meta.resolve('vite/package.json'))), 'bin/vite.js')
const environment = {
  ...process.env,
  PATH: `${join(runtime, 'usr/bin')}:${process.env.PATH}`,
  LD_LIBRARY_PATH: libraries,
  FONTCONFIG_PATH: join(runtime, 'etc/fonts'),
  FONTCONFIG_SYSROOT: runtime,
  GSETTINGS_SCHEMA_DIR: join(runtime, 'usr/share/glib-2.0/schemas'),
  PLAYWRIGHT_BROWSERS_PATH: browsers,
  PLAYWRIGHT_HTML_OUTPUT_DIR: join(evidence, 'playwright-report'),
  SIGNALBOX_WEB_PREVIEW_COMMAND: `${quote(node)} ${quote(vite)} preview --host 127.0.0.1 --port 4173`,
  TMPDIR: temporary,
  XDG_CACHE_HOME: join(temporary, 'cache'),
}
for (const entry of readdirSync(join(project, 'e2e'), { recursive: true, withFileTypes: true })) {
  if (entry.isFile() && entry.name.endsWith('.png')) {
    const path = join(entry.parentPath, entry.name)
    chmodSync(path, statSync(path).mode | 0o200)
  }
}
const result = spawnSync(
  node,
  [
    fileURLToPath(import.meta.resolve('@playwright/test/cli')),
    'test',
    '--workers=2',
    '--output',
    join(evidence, 'test-results'),
    ...process.argv.slice(2),
  ],
  { cwd: project, env: environment, stdio: 'inherit' },
)
if (process.argv.includes('--update-snapshots=all')) {
  for (const entry of readdirSync(join(project, 'e2e'), { withFileTypes: true })) {
    if (entry.isDirectory() && entry.name.endsWith('-snapshots')) {
      materialize(
        join(project, 'e2e', entry.name),
        join(evidence, 'updated-snapshots/e2e', entry.name),
      )
    }
  }
}
if (result.error) throw result.error
if (result.signal) throw new Error(`Playwright exited on ${result.signal}`)
process.exit(result.status ?? 1)
