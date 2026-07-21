// Publish the decmpfs crate to crates.io. Owns the whole flow so CI and a
// local operator run the SAME code: version-parity gate → registry read
// (skip if this version is already live) → cargo publish (dry-run by default;
// pass --publish for the real thing).
//
// Usage:
//   node scripts/repo/publish-crate.mts            # gate + dry-run
//   node scripts/repo/publish-crate.mts --publish  # gate + real publish
//
// Auth: cargo reads CARGO_REGISTRY_TOKEN (CI mints one via crates.io Trusted
// Publishing). SFW_BIN, when set, wraps cargo so registry traffic goes through
// Socket Firewall.

import { readFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
// prefer-async-spawn: sync-required — this is a dep-0 publish gate (the publish
// workflow runs it with no install), so it cannot import the lib spawn; the flow
// is a sequence of synchronous cargo steps.
import { spawnSync } from 'node:child_process'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
const crateDir = path.join(root, 'crates', 'decmpfs')
const publish = process.argv.includes('--publish')

// Dep-0 output helpers: process streams (never `console`, never the lib logger)
// so the publish workflow can run this with only Node on PATH. The stream access
// lives inside a function, so it stays lazy (no module-eval capture).
function err(message: string): void {
  process.stderr.write(`${message}\n`)
}

function out(message: string): void {
  process.stdout.write(`${message}\n`)
}

function run(
  cmd: string,
  args: string[],
  options: { cwd?: string | undefined } = {},
): void {
  const result = spawnSync(cmd, args, { stdio: 'inherit', ...options })
  if (result.status !== 0) {
    err(`publish-crate: \`${cmd}\` exited ${result.status ?? 'on a signal'}.`)
    process.exit(1)
  }
}

// 1. The crate and npm packages never ship a mismatched version.
run(process.execPath, [
  path.join(root, 'scripts', 'repo', 'check-versions.mts'),
])

const version = /^version\s*=\s*"(?<ver>[^"]+)"/m.exec(
  readFileSync(path.join(crateDir, 'Cargo.toml'), 'utf8'),
)?.[1]
if (!version) {
  err(
    'publish-crate: no version in crates/decmpfs/Cargo.toml.\n' +
      '  Saw: no `version = "…"` line. Wanted: a semver.\n' +
      '  Fix: restore the [package] version field.',
  )
  process.exit(1)
}

// 2. Verify state before acting: a crates.io version is permanent. If this
//    version is already live, publishing again can only fail — succeed as a
//    no-op instead of tripping cargo's error.
// Offline / cargo info unavailable — cargo publish itself will still refuse a
// duplicate, so a non-zero status just leaves `live` empty and we continue.
const info = spawnSync('cargo', ['info', 'decmpfs'], {
  encoding: 'utf8',
  stdio: ['ignore', 'pipe', 'ignore'],
})
const live = info.status === 0 ? info.stdout : ''
if (
  new RegExp(`^version: ${version.replaceAll('.', '\\.')}$`, 'm').test(live)
) {
  out(
    `publish-crate: decmpfs@${version} is already on crates.io — nothing to do.`,
  )
  process.exit(0)
}

// 3. Publish (dry-run unless --publish). SFW_BIN wraps cargo when present.
const args = ['publish', '--locked', ...(publish ? [] : ['--dry-run'])]
const sfw = process.env['SFW_BIN']
if (sfw) {
  run(sfw, ['cargo', ...args], { cwd: crateDir })
} else {
  run('cargo', args, { cwd: crateDir })
}
out(
  publish
    ? `publish-crate: decmpfs@${version} published.`
    : `publish-crate: dry run for decmpfs@${version} passed. Re-run with --publish to release.`,
)
