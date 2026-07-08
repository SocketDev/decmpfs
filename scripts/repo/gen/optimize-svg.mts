#!/usr/bin/env node
/*
 * @file Optimize the hand-traced deCMPfs wordmark SVGs with svgo. The wordmark
 *   is a potrace polygon trace: each faceted edge carries many collinear /
 *   near-collinear intermediate points. `convertPathData` collapses those
 *   redundant points, shrinking the files while keeping the exact silhouette.
 *
 *   Deliberately surgical: an explicit plugin list (no `preset-default`) so the
 *   structural rewrites that would break downstream tooling stay OFF —
 *   `logo.mts` extracts the orange `fs` group by its `<g … fill="#f15a24">`
 *   shape, so collapseGroups / mergePaths / moveGroupAttrs must NOT run.
 *   `applyTransforms` stays off too: the group's `scale(0.1,-0.1)` is left
 *   intact and points are simplified in local coords (affine maps preserve
 *   collinearity, so the render is unchanged).
 *
 * Usage:
 *   pnpm run gen:svg              # optimize the source wordmark SVGs in place
 *   pnpm run gen:svg -- --check   # exit non-zero on any drift
 */

import { promises as fs } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

import { optimize } from 'svgo'

const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..')
const ASSETS = path.join(ROOT, 'assets', 'repo')

// The hand-authored source wordmarks. The icon / favicon / PNGs are derived
// from these by `gen:logo`, so they inherit the simplified paths and are not
// optimized here.
const SOURCES = [
  'decmpfs-logo-dark.svg',
  'decmpfs-logo-light.svg',
  'decmpfs-for-rust-dark.svg',
  'decmpfs-for-rust-light.svg',
  'decmpfs-for-npm-dark.svg',
  'decmpfs-for-npm-light.svg',
]

const CONFIG = {
  multipass: true,
  js2svg: { indent: 0, pretty: false },
  plugins: [
    'removeComments',
    'removeMetadata',
    'cleanupAttrs',
    'cleanupNumericValues',
    {
      name: 'convertPathData',
      params: {
        applyTransforms: false,
        floatPrecision: 2,
        makeArcs: false,
      },
    },
    'removeEmptyContainers',
  ],
} as const

async function main(): Promise<void> {
  const check = process.argv.includes('--check')
  let drift = 0
  for (const name of SOURCES) {
    const file = path.join(ASSETS, name)
    const before = await fs.readFile(file, 'utf8')
    const { data: after } = optimize(before, { path: file, ...CONFIG })
    if (after === before) {
      continue
    }
    if (check) {
      console.error(`drift: ${name}`)
      drift += 1
      continue
    }
    await fs.writeFile(file, after)
    const saved = before.length - after.length
    console.log(`optimized: ${name} (-${saved} bytes, ${before.length} -> ${after.length})`)
  }
  if (check && drift > 0) {
    console.error(`${drift} SVG(s) drift — run \`pnpm run gen:svg\`.`)
    process.exitCode = 1
  }
}

main().catch((e: unknown) => {
  console.error(e)
  process.exitCode = 1
})
