// Behavioral coverage of decmpfsStat (napi/decmpfs/src/lib.rs) through the addon.
// Run: node --test. The FS-compression is exercised on the host filesystem: on
// APFS/btrfs/NTFS a compressible write lands `compressed: true`; elsewhere it
// falls back to plain and only the size/round-trip invariants are asserted.

import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import os from 'node:os'
import path from 'node:path'
import { test } from 'node:test'

// The addon's `module.exports = load()` is built dynamically, so it can't be
// statically named-imported under ESM — require it (matches the sibling tests).
const require = createRequire(import.meta.url)
const { decmpfsStat, writeDecmpfsFile } = require('../index.cjs')

test('decmpfsStat reports { compressed, logical, physical } for a plain file', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'decmpfs-stat-'))
  try {
    const filePath = path.join(dir, 'f')
    writeFileSync(filePath, Buffer.alloc(4096))
    const s = decmpfsStat(filePath)
    assert.equal(typeof s.compressed, 'boolean')
    assert.equal(s.logical, 4096)
    assert.ok(s.physical > 0, 'allocated bytes reported')
    assert.equal(
      s.compressed,
      false,
      'a freshly-written plain file is not compressed',
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('decmpfsStat reflects a compressed write where the FS supports it', async () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'decmpfs-stat-c-'))
  try {
    const filePath = path.join(dir, 'addon.node')
    const content = Buffer.alloc(128 * 1024, 0xab)
    const result = await writeDecmpfsFile(filePath, content)
    const s = decmpfsStat(filePath)
    assert.equal(s.logical, content.length, 'logical == the written bytes')
    assert.deepEqual(readFileSync(filePath), content, 'content round-trips')
    if (result.compressed) {
      assert.equal(
        s.compressed,
        true,
        'a compressed write → stat reports compressed',
      )
      assert.ok(
        s.physical < s.logical,
        'allocation shrank below the logical size',
      )
    }
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('decmpfsStat throws a shaped error for a missing path', () => {
  assert.throws(() =>
    decmpfsStat(path.join(os.tmpdir(), 'decmpfs-no-such-stat-xyz')),
  )
})
