#!/usr/bin/env node
/*
 * @file Generate the deCMPfs combomark (shield mark + "by socket labs"
 *   tagline) from the committed `decmpfs-mark.svg`.
 *
 *   Emits an adaptive `decmpfs-combomark.svg` whose tagline flips for
 *   light/dark via `prefers-color-scheme`, plus forced `-light` / `-dark`
 *   copies for contexts that can't honor the media query.
 *
 *   Palette (grounded in the committed brand): mark orange anchored on
 *   #f15a24, a controlled brick-red `fs` accent, and a tagline in brand
 *   gray/cream (dark) or gray/ink (light) — never the AI-slop violet.
 *
 * Usage:
 *   node assets/brand/combomark.mts
 */

import { readFileSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'

const logger = getDefaultLogger()
const HERE = path.dirname(fileURLToPath(import.meta.url))

// --- geometry ---------------------------------------------------------------
const VIEWBOX = 1254
const TAG_SIZE = 60
// "by" and "socket labs" are independent fields.
const BY = { x: 880, y: 1112 }
const SL = { x: 800, y: 1188 }

// --- tagline palette (mark orange/red reads on both; only text flips) -------
const LIGHT = { by: '#736E67', socket: '#4A453E', labs: '#1A1626' }
const DARK = { by: '#9A948C', socket: '#C9C3BB', labs: '#F5F2EC' }

const mark = readFileSync(path.join(HERE, 'decmpfs-mark.svg'), 'utf8')
const paths = mark.match(/<path d="[^"]+"\/?>/g) ?? []
if (paths.length !== 8) {
  logger.error(`expected 8 mark paths, found ${paths.length}`)
  process.exit(1)
}
// First six paths are the shield + d e c m p (orange); the last two are f s.
const markPaths = paths.slice(0, 6)
const fsPaths = paths.slice(6)

/** Y coordinates of a path list (numbers are `x y x y …`, so every 2nd one). */
function yValues(ps: string[]): number[] {
  return ps.flatMap(p =>
    (p.match(/-?\d*\.?\d+/g) ?? []).map(Number).filter((_, i) => i % 2 === 1),
  )
}
const allY = yValues(paths)
const fsY = yValues(fsPaths)
const [y0, y1] = [Math.min(...allY), Math.max(...allY)]
const [fy0, fy1] = [Math.min(...fsY), Math.max(...fsY)]

const DEFS = `  <defs>
    <!-- brand orange, anchored on #f15a24 -->
    <linearGradient id="orange" gradientUnits="userSpaceOnUse" x1="0" y1="${y0.toFixed(0)}" x2="0" y2="${y1.toFixed(0)}">
      <stop offset="0" stop-color="#FF854A"/><stop offset="0.5" stop-color="#F15A24"/><stop offset="1" stop-color="#D8431A"/>
    </linearGradient>
    <!-- controlled brick-red fs accent -->
    <linearGradient id="red" gradientUnits="userSpaceOnUse" x1="0" y1="${fy0.toFixed(0)}" x2="0" y2="${fy1.toFixed(0)}">
      <stop offset="0" stop-color="#EF4A1C"/><stop offset="1" stop-color="#C42711"/>
    </linearGradient>
  </defs>`

const BODY = `  <g fill="url(#orange)">
${markPaths.map(p => `    ${p}`).join('\n')}
  </g>
  <g fill="url(#red)">
${fsPaths.map(p => `    ${p}`).join('\n')}
  </g>`

type Mode = 'adaptive' | 'light' | 'dark'

function tagline(mode: Mode): string {
  let style = ''
  let by: string
  let socket: string
  let labs: string
  if (mode === 'adaptive') {
    style =
      `  <style>.t-by{fill:${LIGHT.by}}.t-so{fill:${LIGHT.socket}}.t-la{fill:${LIGHT.labs}}` +
      `@media(prefers-color-scheme:dark){.t-by{fill:${DARK.by}}.t-so{fill:${DARK.socket}}.t-la{fill:${DARK.labs}}}</style>\n`
    by = 'class="t-by"'
    socket = 'class="t-so"'
    labs = 'class="t-la"'
  } else {
    const c = mode === 'light' ? LIGHT : DARK
    by = `fill="${c.by}"`
    socket = `fill="${c.socket}"`
    labs = `fill="${c.labs}"`
  }
  return (
    style +
    `  <g font-family="Helvetica Neue, Helvetica, Arial, sans-serif" font-size="${TAG_SIZE}" font-weight="600" letter-spacing="3" text-anchor="start">\n` +
    `    <text x="${BY.x}" y="${BY.y}" ${by}>by</text>\n` +
    `    <text x="${SL.x}" y="${SL.y}"><tspan ${socket}>socket</tspan><tspan dx="18" ${labs}>labs</tspan></text>\n` +
    `  </g>`
  )
}

function build(mode: Mode): string {
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${VIEWBOX} ${VIEWBOX}" role="img" aria-label="decmpfs by socket labs">\n` +
    `${DEFS}\n${BODY}\n${tagline(mode)}\n</svg>\n`
  )
}

const outputs: Array<[string, Mode]> = [
  ['decmpfs-combomark.svg', 'adaptive'],
  ['decmpfs-combomark-light.svg', 'light'],
  ['decmpfs-combomark-dark.svg', 'dark'],
]
for (const [name, mode] of outputs) {
  writeFileSync(path.join(HERE, name), build(mode))
}
logger.info(
  `wrote ${outputs.map(([n]) => n).join(', ')} (adaptive flips via prefers-color-scheme)`,
)
