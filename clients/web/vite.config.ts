import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

export default defineConfig({
  plugins: [
    react({ compiler: true }),
    tailwindcss(),
    {
      name: 'retain-prepaint-render-blocking',
      transformIndexHtml: {
        order: 'post',
        handler: (html) =>
          html.replace(
            /<script type="module" crossorigin/g,
            '<script type="module" blocking="render" crossorigin',
          ),
      },
    },
  ],
  test: {
    include: ['src/**/*.test.ts'],
  },
})
