// Behavioral coverage of the N-API binding (napi/decmpfs/src/lib.rs) — every option branch
// of resolve()/run() and the reachable Outcome mappings, driven through the addon.
// Run: node --test (zero-install). The FS-compression itself is exercised on the
// host filesystem; on APFS/btrfs/NTFS compressible data lands `compressed: true`,
// elsewhere it falls back to a plain write (still asserted to round-trip).

import assert from 'node:assert/strict'
import crypto from 'node:crypto'
import { existsSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { mkdtemp, rm } from 'node:fs/promises'
import { createRequire } from 'node:module'
import os from 'node:os'
import path from 'node:path'
import { finished } from 'node:stream/promises'
import { after, before, test } from 'node:test'

const require = createRequire(import.meta.url)
const decmpfs = require('../index.cjs')

let dir
before(async () => {
  dir = await mkdtemp(path.join(os.tmpdir(), 'decmpfs-node-'))
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

test('stream: writes multiple chunks and publishes atomically', async () => {
  const p = path.join(dir, 'stream.node')
  const stream = decmpfs.createDecmpfsWriteStream(p, {
    glob: '**/*.node',
    size: compressible.length,
  })
  stream.write(compressible.subarray(0, 123_456))
  stream.end(compressible.subarray(123_456))
  await finished(stream)
  assert.equal(readFileSync(p).equals(compressible), true)
  assert.equal(stream.result.before, compressible.length)
})

test('stream: incompressible input falls back to an identical plain file', async () => {
  const p = path.join(dir, 'stream-random.node')
  const random = crypto.randomBytes(300_000)
  const stream = decmpfs.createDecmpfsWriteStream(p, { size: random.length })
  for (let offset = 0; offset < random.length; offset += 17_003) {
    stream.write(random.subarray(offset, offset + 17_003))
  }
  stream.end()
  await finished(stream)
  assert.equal(readFileSync(p).equals(random), true)
  assert.match(stream.result.reason, /NoGain|Unsupported/)
})

test('stream: an incomplete download leaves the existing destination untouched', async () => {
  const p = path.join(dir, 'stream-incomplete.node')
  writeFileSync(p, 'existing')
  const stream = decmpfs.createDecmpfsWriteStream(p, { size: 10 })
  stream.end(Buffer.from('short'))
  await assert.rejects(finished(stream), /incomplete stream/)
  assert.equal(readFileSync(p, 'utf8'), 'existing')
})

test('sync: atomic default — writes + round-trips, returns an Outcome', () => {
  const p = path.join(dir, 'sync-default.node')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible, { glob: '**/*.node' })
  assert.equal(readFileSync(p).equals(compressible), true, 'round-trips')
  assert.equal(r.before, compressible.length)
  assert.equal(typeof r.compressed, 'boolean')
  assert.match(r.reason, /Compressed|NoGain|Unsupported/)
})

test('sync: atomic:false — direct write, round-trips', () => {
  const p = path.join(dir, 'sync-direct.node')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible, {
    atomic: false,
    glob: '**/*.node',
  })
  assert.equal(readFileSync(p).equals(compressible), true)
  assert.equal(r.before, compressible.length)
})

test('async: writeDecmpfsFile — writes + round-trips', async () => {
  const p = path.join(dir, 'async.node')
  const r = await decmpfs.writeDecmpfsFile(p, compressible, {
    glob: '**/*.node',
  })
  assert.equal(readFileSync(p).equals(compressible), true)
  assert.equal(r.before, compressible.length)
})

test('no options — defaults (atomic, any path), still writes', () => {
  const p = path.join(dir, 'no-opts.bin')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible)
  assert.equal(readFileSync(p).equals(compressible), true)
  assert.equal(typeof r.compressed, 'boolean')
})

test('gate glob exclude — plain write, Skipped:GateExcluded', () => {
  const p = path.join(dir, 'excluded.txt')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible, { glob: '**/*.node' })
  assert.equal(r.compressed, false)
  assert.equal(r.reason, 'Skipped:GateExcluded')
  assert.equal(readFileSync(p).equals(compressible), true)
})

test('gate size floor exclude — small file under min, plain write', () => {
  const p = path.join(dir, 'small.node')
  const small = Buffer.alloc(1024, 0x42)
  const r = decmpfs.writeDecmpfsFileSync(p, small, {
    glob: '**/*.node',
    minSize: '>= 1MB',
  })
  assert.equal(r.compressed, false)
  assert.equal(r.reason, 'Skipped:GateExcluded')
  assert.equal(readFileSync(p).equals(small), true)
})

test('incompressible data — round-trips (NoGain or compressed-with-no-shrink)', () => {
  const p = path.join(dir, 'random.node')
  const random = crypto.randomBytes(200_000)
  const r = decmpfs.writeDecmpfsFileSync(p, random, { glob: '**/*.node' })
  assert.equal(readFileSync(p).equals(random), true)
  assert.match(r.reason, /NoGain|Compressed|Unsupported/)
})

test('force:false on an existing file — ExistsNoForce, leaves it', () => {
  const p = path.join(dir, 'exists.node')
  decmpfs.writeDecmpfsFileSync(p, compressible, { glob: '**/*.node' })
  const r = decmpfs.writeDecmpfsFileSync(p, Buffer.from('new'), {
    force: false,
  })
  assert.equal(r.reason, 'ExistsNoForce')
  assert.equal(readFileSync(p).equals(compressible), true, 'original untouched')
})

test('errorOnExist on an existing file — throws', () => {
  const p = path.join(dir, 'exists2.node')
  decmpfs.writeDecmpfsFileSync(p, compressible)
  assert.throws(
    () =>
      decmpfs.writeDecmpfsFileSync(p, compressible, {
        force: false,
        errorOnExist: true,
      }),
    /already exists/,
  )
})

test('invalid gate size predicate — throws', () => {
  const p = path.join(dir, 'bad-gate.node')
  assert.throws(
    () =>
      decmpfs.writeDecmpfsFileSync(p, compressible, { minSize: 'not-a-size' }),
    /invalid gate/,
  )
  assert.equal(existsSync(p), false, 'nothing written on a bad gate')
})

test('a write into a missing directory throws a Node-shaped ENOENT', () => {
  const missing = path.join(dir, 'no-such-subdir', 'out.node')
  assert.throws(
    () => decmpfs.writeDecmpfsFileSync(missing, compressible),
    (err: NodeJS.ErrnoException) => {
      assert.equal(err.code, 'ENOENT')
      assert.equal(err.errno, -2)
      assert.equal(err.syscall, 'open')
      return true
    },
  )
})

test('async: a write into a missing directory rejects with a Node-shaped ENOENT', async () => {
  const missing = path.join(dir, 'no-such-subdir', 'out-async.node')
  await assert.rejects(
    decmpfs.writeDecmpfsFile(missing, compressible),
    (err: NodeJS.ErrnoException) => {
      assert.equal(err.code, 'ENOENT')
      assert.equal(err.syscall, 'open')
      return true
    },
  )
})

test('on APFS the compressible file is smaller on disk than its logical size', () => {
  const p = path.join(dir, 'apfs.node')
  const r = decmpfs.writeDecmpfsFileSync(p, compressible, { glob: '**/*.node' })
  if (r.compressed && r.reason === 'Compressed') {
    const st = statSync(p)
    assert.ok(
      st.blocks * 512 < st.size,
      'on-disk allocation below logical size',
    )
  }
})
