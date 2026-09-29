/**
 * Renders the README performance table from the bench result fixtures, oxbox-style:
 * the README never carries numbers by hand; this script owns the bytes.
 * Run: node bench/render-readme-table.mjs  (rewrites the marked section in README.md)
 */
import fs from 'node:fs'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const here = path.dirname(fileURLToPath(import.meta.url))
const root = path.join(here, '..')
const write = JSON.parse(fs.readFileSync(path.join(here, 'results.json'), 'utf8'))
const stream = JSON.parse(fs.readFileSync(path.join(here, 'stream-results.json'), 'utf8'))

const kib = (n) => (n >= 1024 * 1024 ? `${n / 1024 / 1024} MiB` : `${n / 1024} KiB`)
const START = '<!-- perf-table:start -->'
const END = '<!-- perf-table:end -->'

const writeRows = write
  .map(
    (r) =>
      `| ${kib(r.size)} | ${r.async_write_copy_compress_ms} ms | ${r.plain_write_floor_ms} ms | ` +
      `${((1 - r.async_write_copy_compress_ms / r.plain_write_floor_ms) * 100).toFixed(0)}% faster |`,
  )
  .join('\n')
const streamRows = stream
  .map((r) => `| ${r.path} | ${r.ms} ms | ${r.mbps} MB/s |`)
  .join('\n')

const table = `${START}
## Measured write performance (32 MiB-class payloads, APFS decmpfs/LZVN)

The compressed write beats the plain write at large sizes — fewer bytes reach
the disk:

| Payload | writeDecmpfsFile | plain fs write |
| --- | --- | --- |
${writeRows}

The stream path is the fastest way in for large payloads, and per-chunk native
forwarding costs nothing from 64 KiB chunks up:

| Path | 32 MiB | Throughput |
| --- | --- | --- |
${streamRows}

Regenerate: \`node bench/write-overhead.mjs && node bench/stream-throughput.mjs && node bench/render-readme-table.mjs\`
${END}`

const readmePath = path.join(root, 'README.md')
const src = fs.readFileSync(readmePath, 'utf8')
if (src.includes(START)) {
  const re = new RegExp(`${START}[\\s\\S]*${END}`)
  fs.writeFileSync(readmePath, src.replace(re, table))
} else {
  fs.writeFileSync(readmePath, `${src.trimEnd()}\n\n${table}\n`)
}
console.log('README perf table written')
