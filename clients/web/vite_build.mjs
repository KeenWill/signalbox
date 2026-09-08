import { cpSync, mkdtempSync, readdirSync, realpathSync, rmSync, symlinkSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { build } from 'vite'

const source = process.cwd()
const root = mkdtempSync(join(tmpdir(), 'signalbox-vite-'))
try {
  for (const entry of readdirSync(source)) {
    if (entry === 'node_modules' || entry === 'dist') continue
    cpSync(join(source, entry), join(root, entry), { recursive: true, dereference: true })
  }
  symlinkSync(realpathSync(join(source, 'node_modules')), join(root, 'node_modules'))
  await build({ root, build: { outDir: join(source, 'dist') } })
} finally {
  rmSync(root, { recursive: true, force: true })
}
