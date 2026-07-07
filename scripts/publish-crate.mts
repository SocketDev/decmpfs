// Publish the decmpfs crate to crates.io. Owns the whole flow so CI and a
// local operator run the SAME code: version-parity gate → registry read
// (skip if this version is already live) → cargo publish (dry-run by default;
// pass --publish for the real thing).
//
// Usage:
//   node scripts/publish-crate.mts            # gate + dry-run
//   node scripts/publish-crate.mts --publish  # gate + real publish
//
// Auth: cargo reads CARGO_REGISTRY_TOKEN (CI mints one via crates.io Trusted
// Publishing). SFW_BIN, when set, wraps cargo so registry traffic goes through
// Socket Firewall.

import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..')
const crateDir = path.join(root, 'crates', 'decmpfs')
const publish = process.argv.includes('--publish')

function run(cmd: string, args: string[], options: { cwd?: string } = {}): void {
  execFileSync(cmd, args, { stdio: 'inherit', ...options })
}

// 1. The crate and npm packages never ship a mismatched version.
run(process.execPath, [path.join(root, 'scripts', 'check-versions.mts')])

const version = /^version\s*=\s*"([^"]+)"/m.exec(
  readFileSync(path.join(crateDir, 'Cargo.toml'), 'utf8'),
)?.[1]
if (!version) {
  console.error(
    'publish-crate: no version in crates/decmpfs/Cargo.toml.\n' +
      '  Saw: no `version = "…"` line. Wanted: a semver.\n' +
      '  Fix: restore the [package] version field.',
  )
  process.exit(1)
}

// 2. Verify state before acting: a crates.io version is permanent. If this
//    version is already live, publishing again can only fail — succeed as a
//    no-op instead of tripping cargo's error.
let live = ''
try {
  live = execFileSync('cargo', ['info', 'decmpfs'], {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'ignore'],
  })
} catch {
  // Offline / cargo info unavailable — cargo publish itself will still
  // refuse a duplicate, so continue.
}
if (new RegExp(`^version: ${version.replaceAll('.', '\\.')}$`, 'm').test(live)) {
  console.log(`publish-crate: decmpfs@${version} is already on crates.io — nothing to do.`)
  process.exit(0)
}

// 3. Publish (dry-run unless --publish). SFW_BIN wraps cargo when present.
const args = ['publish', '--locked', ...(publish ? [] : ['--dry-run'])]
const sfw = process.env.SFW_BIN
if (sfw) {
  run(sfw, ['cargo', ...args], { cwd: crateDir })
} else {
  run('cargo', args, { cwd: crateDir })
}
console.log(
  publish
    ? `publish-crate: decmpfs@${version} published.`
    : `publish-crate: dry run for decmpfs@${version} passed. Re-run with --publish to release.`,
)
