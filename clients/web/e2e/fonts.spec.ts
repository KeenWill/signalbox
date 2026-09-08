import { expect, test } from './fontTest'

test('uses bundled sans and monospace faces independently of host font selection', async ({
  page,
}, testInfo) => {
  await page.goto('/settings')
  const typography = await page.evaluate(async () => {
    const sans = '14px "Signalbox Test Sans"'
    const mono = '14px "Signalbox Test Mono"'
    await document.fonts.load(sans)
    await document.fonts.load(mono)
    const canvas = document.createElement('canvas')
    const context = canvas.getContext('2d')
    if (context === null) throw new Error('Canvas text measurement is unavailable')
    context.font = sans
    const sansWidth = context.measureText('Signalbox bounded workstation').width
    context.font = mono
    const artifactSummary = document.createElement('p')
    artifactSummary.className = 'artifact-bound-summary'
    document.body.append(artifactSummary)
    const artifactFamily = getComputedStyle(artifactSummary).fontFamily
    artifactSummary.remove()
    return {
      artifactFamily,
      rootFamily: getComputedStyle(document.documentElement).fontFamily,
      sansLoaded: document.fonts.check(sans),
      monoLoaded: document.fonts.check(mono),
      sansWidth,
      monoWidth: context.measureText('Signalbox bounded workstation').width,
    }
  })
  expect(typography.rootFamily).toContain('Signalbox Test Sans')
  expect(typography.sansLoaded).toBe(true)
  expect(typography.monoLoaded).toBe(true)
  expect(typography.artifactFamily).toContain('Signalbox Test Mono')
  await testInfo.attach('font-metrics', {
    body: JSON.stringify(typography),
    contentType: 'application/json',
  })
})
