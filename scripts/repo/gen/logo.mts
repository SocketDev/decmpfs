#!/usr/bin/env node
/*
 * @file Generate deCMPfs raster assets from the hand-traced SVG wordmarks in
 *   `assets/repo/`. The wordmark itself is a vector trace of the source
 *   lettering (committed as `decmpfs-*.svg`); this script derives the square
 *   `fs` icon and rasterizes the favicon / GitHub+npm avatars / logo PNG sizes
 *   via `@resvg/resvg-wasm`. Run after editing the traced SVGs.
 *
 * Usage:
 *   pnpm run gen:logo            # (re)write the icon + PNG kit
 *   pnpm run gen:logo -- --check # exit non-zero on any drift
 */

import { existsSync, promises as fs } from 'node:fs'
import { createRequire } from 'node:module'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

import { initWasm, Resvg } from '@resvg/resvg-wasm'
import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'

const logger = getDefaultLogger()

const require = createRequire(import.meta.url)
const ROOT = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  '..',
)
const ASSETS = path.join(ROOT, 'assets', 'repo')
const PLATE = '#1a1626'

let wasmReady: Promise<void> | undefined
async function ensureWasm(): Promise<void> {
  if (!wasmReady) {
    // Resolve via the main entry (the package's exports map hides package.json);
    // index_bg.wasm sits beside the resolved module.
    const bytes = await fs.readFile(
      path.join(
        path.dirname(require.resolve('@resvg/resvg-wasm')),
        'index_bg.wasm',
      ),
    )
    wasmReady = initWasm(bytes)
  }
  await wasmReady
}

async function rasterize(svg: string, width: number): Promise<Buffer> {
  await ensureWasm()
  return Buffer.from(
    new Resvg(svg, { fitTo: { mode: 'width', value: width } }).render().asPng(),
  )
}

// Pull the orange `fs` group out of the traced dark logo (fill="#f15a24").
async function fsGroup(): Promise<string> {
  const logo = await fs.readFile(
    path.join(ASSETS, 'decmpfs-logo-dark.svg'),
    'utf8',
  )
  const match = logo.match(
    /<g transform="[^"]*"[^>]*fill="#f15a24"[^>]*>[\s\S]*?<\/g>/,
  )
  if (!match) {
    throw new Error(
      'could not find the orange fs group in decmpfs-logo-dark.svg',
    )
  }
  return match[0]
}

// Square icon: brand plate + the traced `fs`, centered. `fs` sits at
// x799..996, y35..221 in the 996x256 wordmark space (measured once); scale 0.91
// fits it into a 256 tile with padding.
function iconSvg(group: string, options: { plate: boolean }): string {
  const opts = { __proto__: null, ...options }
  const bg = opts.plate
    ? `<rect width="256" height="256" rx="48" fill="${PLATE}"/>`
    : ''
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" role="img" aria-label="deCMPfs">${bg}<g transform="translate(-688.5 11.1) scale(0.91)">${group}</g></svg>\n`
}

type Svg = { kind: 'svg'; name: string; body: string }
type Png = { kind: 'png'; name: string; source: string; width: number }

async function plan(): Promise<Array<Svg | Png>> {
  const fs2 = await fsGroup()
  const iconPlate = iconSvg(fs2, { plate: true })
  const iconFlat = iconSvg(fs2, { plate: false })
  const logoDark = await fs.readFile(
    path.join(ASSETS, 'decmpfs-logo-dark.svg'),
    'utf8',
  )
  const logoLight = await fs.readFile(
    path.join(ASSETS, 'decmpfs-logo-light.svg'),
    'utf8',
  )
  const out: Array<Svg | Png> = [
    { kind: 'svg', name: 'decmpfs-icon.svg', body: iconPlate },
    { kind: 'svg', name: 'favicon.svg', body: iconFlat },
  ]
  for (const size of [16, 32, 48, 180]) {
    out.push({
      kind: 'png',
      name: `favicon-${size}.png`,
      source: iconPlate,
      width: size,
    })
  }
  out.push({
    kind: 'png',
    name: 'avatar/decmpfs-avatar-github-500.png',
    source: iconPlate,
    width: 500,
  })
  out.push({
    kind: 'png',
    name: 'avatar/decmpfs-avatar-npm-460.png',
    source: iconPlate,
    width: 460,
  })
  for (const width of [420, 840, 1680]) {
    out.push({
      kind: 'png',
      name: `decmpfs-logo-dark-${width}.png`,
      source: logoDark,
      width,
    })
    out.push({
      kind: 'png',
      name: `decmpfs-logo-light-${width}.png`,
      source: logoLight,
      width,
    })
  }
  return out
}

async function main(): Promise<void> {
  const check = process.argv.includes('--check')
  let drift = 0
  for (const item of await plan()) {
    const target = path.join(ASSETS, item.name)
    const desired =
      item.kind === 'svg'
        ? Buffer.from(item.body, 'utf8')
        : await rasterize(item.source, item.width)
    const current = existsSync(target) ? await fs.readFile(target) : undefined
    if (current?.equals(desired)) {
      continue
    }
    if (check) {
      logger.fail(`drift: ${item.name}`)
      drift += 1
      continue
    }
    await fs.mkdir(path.dirname(target), { recursive: true })
    await fs.writeFile(target, desired)
    logger.log(`wrote: ${item.name}`)
  }
  if (check && drift > 0) {
    logger.fail(`${drift} asset(s) drift — run \`pnpm run gen:logo\`.`)
    process.exitCode = 1
  }
}

main().catch((e: unknown) => {
  logger.fail(e)
  process.exitCode = 1
})
