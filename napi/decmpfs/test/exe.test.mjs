// Behavioral coverage of the pack-executable bindings (packExecutable{,Sync}) —
// the self-replacing-executable packer exposed for a Node-side packing step.
// Run: node --test (zero-install). Exercises the binding's read/compress/
// inject/write/gate path; `stub` is a REQUIRED option (the Node host is not a
// self-replacing runtime — the real end-to-end self-replace is covered by the
// crate's exe_selfreplace integration test against the built decmpfs-stub).

import assert from 'node:assert/strict'
import { chmodSync, existsSync, statSync, writeFileSync } from 'node:fs'
import { mkdtemp, rm } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { after, before, test } from 'node:test'

const require = createRequire(import.meta.url)
const decmpfs = require('../index.cjs')

let dir
let stub
before(async () => {
  dir = await mkdtemp(path.join(tmpdir(), 'decmpfs-exe-'))
  // A synthetic non-Mach-O stub: exercises the binding's inject/write path (the
  // packer routes a non-Mach-O stub through the EOF-footer append on any host)
  // without needing a real object file with header slack. Not runnable — these
  // tests assert the pack RESULT, not a self-replace.
  stub = path.join(dir, 'stub.bin')
  writeFileSync(stub, Buffer.concat([Buffer.from('\x7fELF'), Buffer.alloc(64)]))
})
after(async () => {
  if (dir) {
    await rm(dir, { force: true, recursive: true })
  }
})

// A tiny real executable: a shell script, not a compiled binary, but a valid
// argv[0] target with a real execute bit — enough to exercise the packer's
// read/compress/inject/write path end to end.
function writeTinyExecutable(name) {
  const src = path.join(dir, name)
  writeFileSync(src, '#!/bin/sh\necho hi\n')
  chmodSync(src, 0o755)
  return src
}

test('exports the pack functions', () => {
  assert.equal(typeof decmpfs.packExecutable, 'function')
  assert.equal(typeof decmpfs.packExecutableSync, 'function')
})

test('sync: packs a tiny executable, dest lands executable', () => {
  const src = writeTinyExecutable('sync-src.sh')
  const dest = path.join(dir, 'sync-dest.bin')
  const before = statSync(src).size

  const r = decmpfs.packExecutableSync(src, dest, { stub })

  assert.equal(r.skippedGate, false, 'no gate options passed — must not skip')
  assert.equal(r.packed, true)
  assert.equal(existsSync(dest), true, 'dest was written')
  assert.equal(r.before, before)
  assert.ok(r.after > 0, 'packed size is reported')

  const mode = statSync(dest).mode
  assert.notEqual(mode & 0o111, 0, 'packed executable must carry the execute bit')
})

test('async: packExecutable — packs and round-trips the same result shape', async () => {
  const src = writeTinyExecutable('async-src.sh')
  const dest = path.join(dir, 'async-dest.bin')
  const before = statSync(src).size

  const r = await decmpfs.packExecutable(src, dest, { stub })

  assert.equal(existsSync(dest), true, 'dest was written')
  assert.equal(r.packed, true)
  assert.equal(r.skippedGate, false)
  assert.equal(r.before, before)
  assert.ok(r.after > 0)
})

test('gateGlob miss — skippedGate true, nothing written', () => {
  const src = writeTinyExecutable('gate-src.sh')
  const dest = path.join(dir, 'gate-dest.bin')

  const r = decmpfs.packExecutableSync(src, dest, { stub, gateGlob: '*.never-matches' })

  assert.equal(r.packed, false)
  assert.equal(r.skippedGate, true)
  assert.equal(r.before, 0)
  assert.equal(r.after, 0)
  assert.equal(existsSync(dest), false, 'a gate miss must write nothing')
})

test('gateSize floor miss on a tiny source — skippedGate true', () => {
  const src = writeTinyExecutable('gate-size-src.sh')
  const dest = path.join(dir, 'gate-size-dest.bin')

  const r = decmpfs.packExecutableSync(src, dest, { stub, gateSize: '>= 1MB' })

  assert.equal(r.skippedGate, true)
  assert.equal(existsSync(dest), false)
})

test('invalid gate size predicate — throws', () => {
  const src = writeTinyExecutable('bad-gate-src.sh')
  const dest = path.join(dir, 'bad-gate-dest.bin')

  assert.throws(
    () => decmpfs.packExecutableSync(src, dest, { stub, gateSize: 'not-a-size' }),
    /invalid gate/,
  )
  assert.equal(existsSync(dest), false, 'nothing written on a bad gate')
})
