/**
 * The stream-path measurement: does per-chunk native forwarding cost anything?
 * Compares createDecmpfsWriteStream at several chunk sizes against the single-shot
 * writeDecmpfsFile and a raw fs write stream on the same payload.
 *
 * Run: copy the built addon as bench/decmpfs.node, then node bench/stream-throughput.mjs.
 */
import fs from 'node:fs'
import { unlinkSync } from 'node:fs'
import { pipeline } from 'node:stream/promises'
import { Readable } from 'node:stream'
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const require = createRequire(import.meta.url)
const here = path.dirname(fileURLToPath(import.meta.url))
const addon = require(path.join(here, '..', 'napi', 'decmpfs', 'index.cjs'))

const SIZE = 32 * 1024 * 1024
const REPS = 15
const block = Buffer.from('compressible chunk 0123456789 ')
const payload = Buffer.alloc(SIZE)
for (let i = 0; i < SIZE; i += block.length) block.copy(payload, i)

function chunkSource(data, chunkSize) {
  let offset = 0
  return new Readable({
    read() {
      if (offset >= data.length) return this.push(null)
      this.push(data.subarray(offset, Math.min(offset + chunkSize, data.length)))
      offset += chunkSize
    },
  })
}

async function time(fn) {
  const t0 = process.hrtime.bigint()
  await fn()
  return Number(process.hrtime.bigint() - t0) / 1e6
}

const dir = path.join(here, 'stream-work')
fs.mkdirSync(dir, { recursive: true })
const rows = []
for (const chunk of [16 * 1024, 64 * 1024, 512 * 1024, 2 * 1024 * 1024]) {
  const p = path.join(dir, `s-${chunk}.bin`)
  // Warmup.
  await pipeline(chunkSource(payload, chunk), addon.createDecmpfsWriteStream(p, { size: SIZE, force: true }))
  if (fs.existsSync(p)) unlinkSync(p)
  let acc = 0
  for (let i = 0; i < REPS; i++) {
    acc += await time(() => pipeline(chunkSource(payload, chunk), addon.createDecmpfsWriteStream(p, { size: SIZE, force: true })))
    if (fs.existsSync(p)) unlinkSync(p)
  }
  const ms = +(acc / REPS).toFixed(2)
  rows.push({ path: `stream ${chunk / 1024} KiB chunks`, ms, mbps: +(SIZE / 1024 / 1024 / (ms / 1000)).toFixed(0) })
}
// Single-shot + raw floors.
{
  const p = path.join(dir, 'single.bin')
  await addon.writeDecmpfsFile(p, payload, { force: true })
  if (fs.existsSync(p)) unlinkSync(p)
  let acc = 0
  for (let i = 0; i < REPS; i++) {
    acc += await time(() => addon.writeDecmpfsFile(p, payload, { force: true }))
    if (fs.existsSync(p)) unlinkSync(p)
  }
  const ms = +(acc / REPS).toFixed(2)
  rows.push({ path: 'single-shot writeDecmpfsFile', ms, mbps: +(SIZE / 1024 / 1024 / (ms / 1000)).toFixed(0) })
}
{
  const p = path.join(dir, 'raw.bin')
  await pipeline(chunkSource(payload, 64 * 1024), fs.createWriteStream(p, { flush: true }))
  let acc = 0
  for (let i = 0; i < REPS; i++) {
    acc += await time(() => pipeline(chunkSource(payload, 64 * 1024), fs.createWriteStream(p, { flush: true })))
    if (fs.existsSync(p)) unlinkSync(p)
  }
  const ms = +(acc / REPS).toFixed(2)
  rows.push({ path: 'raw fs write stream', ms, mbps: +(SIZE / 1024 / 1024 / (ms / 1000)).toFixed(0) })
  fs.rmSync(dir, { recursive: true, force: true })
}
fs.writeFileSync(path.join(here, 'stream-results.json'), `${JSON.stringify(rows, undefined, 2)}\n`)
console.table(rows)
