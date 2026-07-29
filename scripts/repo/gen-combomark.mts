#!/usr/bin/env node
/*
 * @file Generate the deCMPfs combomark from the committed
 *   `decmpfs-logomark.svg`. The badge is all-in-one (the wordmark and the
 *   arced SOCKET LABS footer live inside the shield), so the combomark IS the
 *   logomark, re-emitted for every variant slot. The mark is single-color
 *   (`var(--logo, #F15A24)`), so light and dark are the same bytes.
 *
 * Usage:
 *   node scripts/repo/gen-combomark.mts
 */

import { readFileSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'

const logger = getDefaultLogger()
const HERE = path.dirname(fileURLToPath(import.meta.url))
const BRAND_DIR = path.join(HERE, '..', '..', 'assets', 'repo', 'brand')

const mark = readFileSync(path.join(BRAND_DIR, 'decmpfs-logomark.svg'), 'utf8')
if (!mark.includes('<svg') || !mark.includes('<path')) {
  logger.error('expected the committed logomark to be a drawable SVG')
  process.exit(1)
}

export type Mode = 'adaptive' | 'light' | 'dark'

export function buildCombomark(mode: Mode): string {
  void mode
  return mark
}

const outputs: Array<[string, Mode]> = [
  ['decmpfs-combomark.svg', 'adaptive'],
  ['decmpfs-combomark-light.svg', 'light'],
  ['decmpfs-combomark-dark.svg', 'dark'],
]
for (const [name, mode] of outputs) {
  writeFileSync(path.join(BRAND_DIR, name), buildCombomark(mode))
}
logger.info(
  `wrote ${outputs.map(([n]) => n).join(', ')} (all-in-one badge, single-color)`,
)
