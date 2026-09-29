/**
 * The write-path overhead decomposition: where the async compressed write spends
 * its time versus the floors it sits on.
 *
 *   raw write floor            - write precompressed bytes (OS work only)
 *   compress_file_in_place     - engine compresses an existing file (no JS-buffer copy)
 *   sync write (copy+compress) - writeDecmpfsFileSync
 *   async write (copy+compress)- writeDecmpfsFile (the default path)
 *
 * Run: copy the built addon as bench/decmpfs.node, then node bench/write-overhead.mjs.
 * Writes bench/results.json.
 */
import fs from 'node:fs'
import { unlinkSync } from 'node:fs'
import { promises as fsp } from 'node:fs'
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const require = createRequire(import.meta.url)
const here = path.dirname(fileURLToPath(import.meta.url))
const addon = require(path.join(here, 'decmpfs.node'))

const sizes = [512 * 1024, 4 * 1024 * 1024, 32 * 1024 * 1024]
const REPS = 25
const block = Buffer.from('compressible chunk 0123456789 ')
function payload(n) {
  const buf = Buffer.alloc(n)
  for (let i = 0; i < n; i += block.length) block.copy(buf, i)
  return buf
}
async function time(fn) {
  const t0 = process.hrtime.bigint()
  await fn()
  return Number(process.hrtime.bigint() - t0) / 1e6
}
function timeSync(fn) {
  const t0 = process.hrtime.bigint()
  fn()
  return Number(process.hrtime.bigint() - t0) / 1e6
}

const rows = []
for (const size of sizes) {
  const data = payload(size)
  const dir = path.join(here, `work-${size}`)
  fs.mkdirSync(dir, { recursive: true })
  const dec = path.join(dir, 'dec.bin')
  const plain = path.join(dir, 'plain.bin')
  const pre = path.join(dir, 'pre.bin')

  // Precompute compressed bytes for the raw-write floor.
  await addon.writeDecmpfsFile(pre, data, { force: true })
  const compressedBytes = fs.readFileSync(pre)

  // Warmups.
  await addon.writeDecmpfsFile(dec, data, { force: true })
  addon.writeDecmpfsFileSync(`${dec}w`, data, { force: true })
  unlinkSync(`${dec}w`)
  await fsp.writeFile(plain, data)
  await addon.compressFile(plain, { force: true })

  let asyncMs = 0
  let syncMs = 0
  let rawMs = 0
  let compressFileMs = 0
  let plainMs = 0
  for (let i = 0; i < REPS; i++) {
    asyncMs += await time(() => addon.writeDecmpfsFile(dec, data, { force: true }))
    syncMs += timeSync(() => addon.writeDecmpfsFileSync(dec, data, { force: true }))
    rawMs += await time(() => fsp.writeFile(pre, compressedBytes))
    await fsp.writeFile(plain, data)
    compressFileMs += await time(() => addon.compressFile(plain, { force: true }))
    plainMs += await time(() => fsp.writeFile(plain, data))
    unlinkSync(dec)
    unlinkSync(plain)
  }
  rows.push({
    size,
    async_write_copy_compress_ms: +(asyncMs / REPS).toFixed(3),
    sync_write_copy_compress_ms: +(syncMs / REPS).toFixed(3),
    compress_file_in_place_ms: +(compressFileMs / REPS).toFixed(3),
    raw_write_floor_ms: +(rawMs / REPS).toFixed(3),
    plain_write_floor_ms: +(plainMs / REPS).toFixed(3),
  })
  fs.rmSync(dir, { recursive: true, force: true })
}
fs.writeFileSync(path.join(here, 'results.json'), `${JSON.stringify(rows, undefined, 2)}\n`)
console.table(rows.map(({ size, ...r }) => ({ size_kib: size / 1024, ...r })))
