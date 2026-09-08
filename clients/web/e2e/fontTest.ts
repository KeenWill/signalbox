import { readFileSync } from 'node:fs'
import { test as base } from '@playwright/test'

export { expect, type Page, type Route, type TestInfo } from '@playwright/test'

const fonts = [
  { family: 'Signalbox Test Sans', weight: '400', file: 'DejaVuSans.ttf' },
  { family: 'Signalbox Test Sans', weight: '700', file: 'DejaVuSans-Bold.ttf' },
  { family: 'Signalbox Test Mono', weight: '400', file: 'DejaVuSansMono.ttf' },
  { family: 'Signalbox Test Mono', weight: '700', file: 'DejaVuSansMono-Bold.ttf' },
].map(({ family, weight, file }) => ({
  family,
  weight,
  source: `url(data:font/ttf;base64,${readFileSync(new URL(`./fonts/${file}`, import.meta.url)).toString('base64')})`,
}))

export const test = base.extend({
  context: async ({ context }, use) => {
    await context.addInitScript((faces) => {
      for (const face of faces) {
        document.fonts.add(new FontFace(face.family, face.source, { weight: face.weight }))
      }
      const typography = new CSSStyleSheet()
      typography.replaceSync(
        ':root { --signalbox-font-sans: "Signalbox Test Sans"; --signalbox-font-mono: "Signalbox Test Mono"; }',
      )
      document.adoptedStyleSheets = [...document.adoptedStyleSheets, typography]
    }, fonts)
    await use(context)
  },
})
