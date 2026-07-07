// Keep index.d.ts honest: every value/function the declarations claim the addon
// exports must actually be exported by index.cjs, and vice versa. The .d.ts is
// hand-maintained (no napi CLI codegen here), so this drift test is the gate
// that catches a rename/add/remove in src/lib.rs that the .d.ts missed.

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'

const here = path.dirname(fileURLToPath(import.meta.url))
const require = createRequire(import.meta.url)
const addon = require('../index.cjs') as Record<string, unknown>

// The runtime truth: every own name the native addon exports.
const actual = new Set(Object.keys(addon))

// The declared truth: `export const NAME` + `export function NAME` in the .d.ts
// (interfaces are type-only and never appear at runtime, so they're excluded).
const dts = readFileSync(path.join(here, '..', 'index.d.ts'), 'utf8')
const declared = new Set<string>()
for (const m of dts.matchAll(/^export (?:declare )?(?:const|function|class) (\w+)/gm)) {
  declared.add(m[1]!)
}

test('every declared export exists on the addon', () => {
  const missing = [...declared].filter(name => !actual.has(name))
  assert.deepEqual(missing, [], `index.d.ts declares names the addon lacks: ${missing}`)
})

test('every addon export is declared in index.d.ts', () => {
  const undeclared = [...actual].filter(name => !declared.has(name))
  assert.deepEqual(
    undeclared,
    [],
    `addon exports names index.d.ts is missing (update index.d.ts): ${undeclared}`,
  )
})
