// Behavioral coverage of the N-API binding (node/src/lib.rs) — every option branch
// of resolve()/run() and the reachable Outcome mappings, driven through the addon.
// Run: node --test (zero-install). The FS-compression itself is exercised on the
// host filesystem; on APFS/btrfs/NTFS compressible data lands `compressed: true`,
// elsewhere it falls back to a plain write (still asserted to round-trip).

import assert from 'node:assert/strict'
import { randomBytes } from 'node:crypto'
import { existsSync, readFileSync, statSync } from 'node:fs'
import { mkdtemp, rm } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { after, before, test } from 'node:test'

const require = createRequire(import.meta.url)
const decmpfs = require('../index.js')

let dir
before(async () => {
  dir = await mkdtemp(join(tmpdir(), 'decmpfs-node-'))
})
after(async () => {
  if (dir) {
    await rm(dir, { force: true, recursive: true })
  }
})

const compressible = Buffer.alloc(300_000, 0x41)

test('exports the two write functions', () => {
  assert.equal(typeof decmpfs.writeDecmpfsFile, 'function')
  assert.equal(typeof decmpfs.writeDecmpfsFileSync, 'function')
})

test('sync: atomic default — writes + round-trips, returns an Outcome', () => {
  const p = join(dir, 'sync-default.node')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible, { glob: '**/*.node' })
  assert.equal(readFileSync(p).equals(compressible), true, 'round-trips')
  assert.equal(r.before, compressible.length)
  assert.equal(typeof r.compressed, 'boolean')
  assert.match(r.reason, /Compressed|NoGain|Unsupported/)
})

test('sync: atomic:false — direct write, round-trips', () => {
  const p = join(dir, 'sync-direct.node')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible, { atomic: false, glob: '**/*.node' })
  assert.equal(readFileSync(p).equals(compressible), true)
  assert.equal(r.before, compressible.length)
})

test('async: writeDecmpfsFile — writes + round-trips', async () => {
  const p = join(dir, 'async.node')
  const r = await decmpfs.writeDecmpfsFile(p, compressible, { glob: '**/*.node' })
  assert.equal(readFileSync(p).equals(compressible), true)
  assert.equal(r.before, compressible.length)
})

test('no options — defaults (atomic, any path), still writes', () => {
  const p = join(dir, 'no-opts.bin')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible)
  assert.equal(readFileSync(p).equals(compressible), true)
  assert.equal(typeof r.compressed, 'boolean')
})

test('gate glob exclude — plain write, Skipped:GateExcluded', () => {
  const p = join(dir, 'excluded.txt')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible, { glob: '**/*.node' })
  assert.equal(r.compressed, false)
  assert.equal(r.reason, 'Skipped:GateExcluded')
  assert.equal(readFileSync(p).equals(compressible), true)
})

test('gate size floor exclude — small file under min, plain write', () => {
  const p = join(dir, 'small.node')
  const small = Buffer.alloc(1024, 0x42)
  const r = decmpfs.writeDecmpfsFileSync(p, small, { glob: '**/*.node', minSize: '>= 1MB' })
  assert.equal(r.compressed, false)
  assert.equal(r.reason, 'Skipped:GateExcluded')
  assert.equal(readFileSync(p).equals(small), true)
})

test('incompressible data — round-trips (NoGain or compressed-with-no-shrink)', () => {
  const p = join(dir, 'random.node')
  const random = randomBytes(200_000)
  const r = decmpfs.writeDecmpfsFileSync(p, random, { glob: '**/*.node' })
  assert.equal(readFileSync(p).equals(random), true)
  assert.match(r.reason, /NoGain|Compressed|Unsupported/)
})

test('force:false on an existing file — ExistsNoForce, leaves it', () => {
  const p = join(dir, 'exists.node')
  decmpfs.writeDecmpfsFileSync(p, compressible, { glob: '**/*.node' })
  const r = decmpfs.writeDecmpfsFileSync(p, Buffer.from('new'), { force: false })
  assert.equal(r.reason, 'ExistsNoForce')
  assert.equal(readFileSync(p).equals(compressible), true, 'original untouched')
})

test('errorOnExist on an existing file — throws', () => {
  const p = join(dir, 'exists2.node')
  decmpfs.writeDecmpfsFileSync(p, compressible)
  assert.throws(
    () => decmpfs.writeDecmpfsFileSync(p, compressible, { force: false, errorOnExist: true }),
    /already exists/,
  )
})

test('invalid gate size predicate — throws', () => {
  const p = join(dir, 'bad-gate.node')
  assert.throws(
    () => decmpfs.writeDecmpfsFileSync(p, compressible, { minSize: 'not-a-size' }),
    /invalid gate/,
  )
  assert.equal(existsSync(p), false, 'nothing written on a bad gate')
})

test('on APFS the compressible file is smaller on disk than its logical size', () => {
  const p = join(dir, 'apfs.node')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible, { glob: '**/*.node' })
  if (r.compressed && r.reason === 'Compressed') {
    const st = statSync(p)
    assert.ok(st.blocks * 512 < st.size, 'on-disk allocation below logical size')
  }
})
