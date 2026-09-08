import { realpathSync } from 'node:fs'
import { resolve } from 'node:path'
import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

const prepaintEntry = realpathSync(resolve(import.meta.dirname, 'src/prepaint.ts'))

export default defineConfig({
  plugins: [
    react({ compiler: true }),
    tailwindcss(),
    {
      name: 'emit-prepaint-entry',
      apply: 'build',
      buildStart() {
        this.emitFile({
          type: 'chunk',
          id: prepaintEntry,
          name: 'prepaint',
          preserveSignature: 'strict',
        })
      },
      transformIndexHtml: {
        order: 'post',
        handler: (html, { bundle }) => {
          const prepaint = Object.values(bundle ?? {}).find(
            (chunk) => chunk.type === 'chunk' && chunk.facadeModuleId === prepaintEntry,
          )
          if (!prepaint) throw new Error('Prepaint entry was not emitted')
          return html.replace('/src/prepaint.ts', `/${prepaint.fileName}`)
        },
      },
    },
  ],
  test: {
    include: ['src/**/*.test.ts'],
  },
})
