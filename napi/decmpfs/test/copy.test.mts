// Behavioral coverage of the copy bindings (copyDecmpfsFile{,Sync},
// copyFile{,Sync} + the COPYFILE_* mode flags) — the compression-preserving
// copy Node's fs.copyFile can't do on macOS (libuv has no clonefile path:
// FICLONE silently byte-copies, FICLONE_FORCE throws ENOSYS).
// Run: node --test (zero-install). On APFS/btrfs the clone path is exercised
// for real; elsewhere the fallback still lands byte-identical output.

import assert from 'node:assert/strict'
import { readFileSync, statSync, writeFileSync } from 'node:fs'
import { mkdtemp, rm } from 'node:fs/promises'
import { createRequire } from 'node:module'
import * as os from 'node:os'
import path from 'node:path'
import { after, before, test } from 'node:test'

const require = createRequire(import.meta.url)
const decmpfs = require('../index.cjs')

let dir
before(async () => {
  dir = await mkdtemp(path.join(os.tmpdir(), 'decmpfs-copy-'))
})
after(async () => {
  if (dir) {
    await rm(dir, { force: true, recursive: true })
  }
})

const compressible = Buffer.alloc(300_000, 0x41)

// A compressed source fixture: written once through the write binding. On a
// compressing FS this lands compressed; the copy tests assert round-trip
// either way and compression-preservation only when the source compressed.
function compressedSource(name) {
  const src = path.join(dir, name)
  const wrote = decmpfs.writeDecmpfsFileSync(src, compressible)
  return { src, compressed: wrote.compressed }
}

test('exports the copy functions and Node-parity mode flags', () => {
  assert.equal(typeof decmpfs.copyDecmpfsFile, 'function')
  assert.equal(typeof decmpfs.copyDecmpfsFileSync, 'function')
  assert.equal(typeof decmpfs.copyFile, 'function')
  assert.equal(typeof decmpfs.copyFileSync, 'function')
  assert.equal(decmpfs.COPYFILE_EXCL, 1)
  assert.equal(decmpfs.COPYFILE_FICLONE, 2)
  assert.equal(decmpfs.COPYFILE_FICLONE_FORCE, 4)
})

test('sync: copy round-trips and preserves compression state', () => {
  const { src, compressed } = compressedSource('copy-sync-src.node')
  const dest = path.join(dir, 'copy-sync-dest.node')
  const r = decmpfs.copyDecmpfsFileSync(src, dest)
  assert.equal(readFileSync(dest).equals(compressible), true, 'round-trips')
  if (compressed) {
    assert.equal(r.compressed, true, 'compression state carried over')
    assert.ok(
      statSync(dest).blocks * 512 < compressible.length,
      'destination is smaller on disk than its logical size',
    )
  }
})

test('async: copy round-trips', async () => {
  const { src } = compressedSource('copy-async-src.node')
  const dest = path.join(dir, 'copy-async-dest.node')
  const r = await decmpfs.copyDecmpfsFile(src, dest)
  assert.equal(readFileSync(dest).equals(compressible), true)
  assert.equal(typeof r.reason, 'string')
})

test('sync: force default replaces an existing destination', () => {
  const { src } = compressedSource('copy-force-src.node')
  const dest = path.join(dir, 'copy-force-dest.node')
  writeFileSync(dest, 'stale')
  decmpfs.copyDecmpfsFileSync(src, dest)
  assert.equal(readFileSync(dest).equals(compressible), true, 'replaced')
})

test('sync: force false reports ExistsNoForce without replacing', () => {
  const { src } = compressedSource('copy-noforce-src.node')
  const dest = path.join(dir, 'copy-noforce-dest.node')
  writeFileSync(dest, 'keep me')
  const r = decmpfs.copyDecmpfsFileSync(src, dest, { force: false })
  assert.equal(r.reason, 'ExistsNoForce')
  assert.equal(readFileSync(dest, 'utf8'), 'keep me', 'not replaced')
})

test('sync: errorOnExist throws on an existing destination', () => {
  const { src } = compressedSource('copy-eoe-src.node')
  const dest = path.join(dir, 'copy-eoe-dest.node')
  writeFileSync(dest, 'occupied')
  assert.throws(
    () => decmpfs.copyDecmpfsFileSync(src, dest, { errorOnExist: true }),
    /already exists/,
  )
})

test('sync: a missing source throws a Node-shaped ENOENT', () => {
  const missing = path.join(dir, 'absent.node')
  assert.throws(
    () => decmpfs.copyDecmpfsFileSync(missing, path.join(dir, 'never.node')),
    (err: NodeJS.ErrnoException) => {
      assert.equal(err.code, 'ENOENT')
      assert.equal(err.errno, -2)
      assert.equal(err.syscall, 'stat')
      assert.equal(err.path, missing)
      return true
    },
  )
})

test('copyFile: COPYFILE_EXCL rejects an existing destination', () => {
  const { src } = compressedSource('copyfile-excl-src.node')
  const dest = path.join(dir, 'copyfile-excl-dest.node')
  writeFileSync(dest, 'occupied')
  assert.throws(
    () => decmpfs.copyFileSync(src, dest, decmpfs.COPYFILE_EXCL),
    /EEXIST/,
  )
})

test('copyFile: default mode copies with compression preserved', async () => {
  const { src, compressed } = compressedSource('copyfile-default-src.node')
  const dest = path.join(dir, 'copyfile-default-dest.node')
  const r = await decmpfs.copyFile(src, dest)
  assert.equal(readFileSync(dest).equals(compressible), true)
  if (compressed) {
    assert.equal(r.compressed, true)
  }
})

test('copyFile: FICLONE mode behaves like default (clone-first, never a compression-dropping copy)', async () => {
  const { src, compressed } = compressedSource('copyfile-ficlone-src.node')
  const dest = path.join(dir, 'copyfile-ficlone-dest.node')
  const r = await decmpfs.copyFile(src, dest, decmpfs.COPYFILE_FICLONE)
  assert.equal(readFileSync(dest).equals(compressible), true)
  if (compressed) {
    assert.equal(r.compressed, true, 'compression state carried over')
  }
})

test('copyFile: FICLONE_FORCE clones on a cloning FS and round-trips', () => {
  const { src, compressed } = compressedSource('copyfile-force-src.node')
  const dest = path.join(dir, 'copyfile-force-dest.node')
  if (process.platform === 'win32' || !compressed) {
    // NTFS supports compression but has no clone primitive; a
    // non-cloning/non-compressing FS likewise cannot satisfy FORCE.
    assert.throws(
      () => decmpfs.copyFileSync(src, dest, decmpfs.COPYFILE_FICLONE_FORCE),
      /ENOTSUP/,
    )
    return
  }
  const r = decmpfs.copyFileSync(src, dest, decmpfs.COPYFILE_FICLONE_FORCE)
  assert.equal(r.reason, 'Cloned')
  assert.equal(readFileSync(dest).equals(compressible), true)
})
