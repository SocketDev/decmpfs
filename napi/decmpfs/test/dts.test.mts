// Runtime half of keeping index.d.cts honest: assert every declared public
// export is actually present on the built addon. A .d.cts can declare an export
// the native addon doesn't ship; tsc can't see that, this can. The TYPE half —
// that the declarations are valid and complete — is `pnpm run typecheck` (native
// tsc against the type-tests/uses-api.ts fixture), run as its own CI job so the
// unit tests stay dependency-free (no tsc, no typescript install).

import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import { test } from 'node:test'

const require = createRequire(import.meta.url)

test('every declared public export is present on the built addon', () => {
  const addon = require('../index.cjs') as Record<string, unknown>
  const functions = [
    'copyDecmpfsFile',
    'copyDecmpfsFileSync',
    'copyFile',
    'copyFileSync',
    'packExecutable',
    'packExecutableSync',
    'writeDecmpfsFile',
    'writeDecmpfsFileSync',
  ]
  for (const name of functions) {
    assert.equal(typeof addon[name], 'function', `addon must export function ${name}`)
  }
  const consts = ['COPYFILE_EXCL', 'COPYFILE_FICLONE', 'COPYFILE_FICLONE_FORCE']
  for (const name of consts) {
    assert.equal(typeof addon[name], 'number', `addon must export const ${name}`)
  }
})
