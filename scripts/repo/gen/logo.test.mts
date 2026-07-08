// Guards the deCMPfs "for X" lockups: the "for" label must stay SMALL relative
// to the wordmark (it regressed to oversized once). Renders each dark lockup via
// @resvg/resvg-wasm and measures the gray "for" pixels' vertical extent as a
// fraction of the image height, asserting it stays in a tight band. Run:
//   node --test scripts/repo/gen/logo.test.mts   (pnpm run test:assets)

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import path from 'node:path'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'

import { initWasm, Resvg } from '@resvg/resvg-wasm'

const require = createRequire(import.meta.url)
const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..')
const ASSETS = path.join(ROOT, 'assets', 'repo')
// The "for" fill on the dark lockups (#b9b3ab).
const FOR_RGB = [0xb9, 0xb3, 0xab]
const TOL = 22

let wasmReady: Promise<void> | undefined
async function ensureWasm(): Promise<void> {
  if (!wasmReady) {
    wasmReady = initWasm(
      readFileSync(path.join(path.dirname(require.resolve('@resvg/resvg-wasm')), 'index_bg.wasm')),
    )
  }
  await wasmReady
}

// Fraction of the rendered image height spanned by pixels matching the "for"
// gray. 0 means the label wasn't found.
async function forHeightRatio(name: string): Promise<number> {
  await ensureWasm()
  const svg = readFileSync(path.join(ASSETS, name), 'utf8')
  const img = new Resvg(svg, { fitTo: { mode: 'width', value: 800 } }).render()
  const { height, pixels, width } = img
  let minY = height
  let maxY = -1
  // A row counts as "for" only if it has a solid run of the flat gray. The cream
  // wordmark's anti-alias edges blend to a near-NEUTRAL gray (R≈G≈B), while the
  // "for" fill #b9b3ab is WARM (R−B≈14); requiring that warmth rejects the cream
  // blend that would otherwise inflate the span up into the wordmark.
  const MIN_RUN = 12
  const WARMTH = 8
  for (let y = 0; y < height; y += 1) {
    let matches = 0
    for (let x = 0; x < width; x += 1) {
      const i = (y * width + x) * 4
      if (
        pixels[i + 3]! >= 128 &&
        Math.abs(pixels[i]! - FOR_RGB[0]!) <= TOL &&
        Math.abs(pixels[i + 1]! - FOR_RGB[1]!) <= TOL &&
        Math.abs(pixels[i + 2]! - FOR_RGB[2]!) <= TOL &&
        pixels[i]! - pixels[i + 2]! >= WARMTH
      ) {
        matches += 1
      }
    }
    if (matches >= MIN_RUN) {
      if (y < minY) {
        minY = y
      }
      if (y > maxY) {
        maxY = y
      }
    }
  }
  return maxY < 0 ? 0 : (maxY - minY + 1) / height
}

for (const variant of ['decmpfs-for-rust-dark.svg', 'decmpfs-for-npm-dark.svg']) {
  test(`${variant}: "for" is present but small`, async () => {
    const ratio = await forHeightRatio(variant)
    assert.ok(ratio > 0.08, `"for" not found / too small in ${variant} (ratio ${ratio.toFixed(3)})`)
    assert.ok(ratio < 0.34, `"for" is oversized in ${variant} (ratio ${ratio.toFixed(3)}, cap 0.34)`)
  })
}
