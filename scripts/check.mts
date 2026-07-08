// Full static gate: format check + clippy (the Rust "typecheck") across the
// workspace. Pre-push and CI both run this so a push never lands what CI
// would reject.
//
//   node scripts/check.mts

import { execFileSync } from 'node:child_process'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..')

function run(label: string, cmd: string, args: string[]): void {
  console.log(`check: ${label}`)
  try {
    execFileSync(cmd, args, { cwd: root, stdio: 'inherit' })
  } catch {
    console.error(`check: ${label} failed.`)
    process.exit(1)
  }
}

run('cargo fmt --check', 'cargo', ['fmt', '--all', '--check'])

// Clippy across the feature matrix. Default features never compile the `addon`
// / `exe` modules, so their panic-free deny
// (`#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]`)
// is only enforced when those features are actually linted — run each.
const FEATURE_SETS = [
  { label: 'default', args: [] },
  { label: 'addon', args: ['--features', 'addon'] },
  { label: 'exe', args: ['--features', 'exe'] },
]
for (const { label, args } of FEATURE_SETS) {
  run(`cargo clippy (${label})`, 'cargo', [
    'clippy',
    '--workspace',
    '--all-targets',
    '--locked',
    ...args,
    '--',
    '-D',
    'warnings',
  ])
}
// Test the feature-gated code too — a green clippy doesn't run the tests, and a
// default-feature `cargo test` never compiles the `addon` / `exe` modules, so
// their unit tests (SHA-512 integrity + length math, self-replace) would
// otherwise ship untested.
run('cargo test (addon)', 'cargo', ['test', '--features', 'addon'])
run('cargo test (exe)', 'cargo', ['test', '--features', 'exe'])
run('version parity', process.execPath, [
  path.join(root, 'scripts', 'check-versions.mts'),
])
// Type-check the hand-maintained napi declarations with the pinned
// typescript (the drift test asserts names exist; tsc asserts the
// declarations themselves are well-formed and strict-clean).
run('tsc --noEmit (napi type declarations)', path.join(root, 'node_modules', '.bin', 'tsc'), [
  '--noEmit',
  '--strict',
  '--target',
  'es2022',
  '--module',
  'nodenext',
  '--moduleResolution',
  'nodenext',
  path.join(root, 'napi', 'decmpfs', 'index.d.cts'),
])
console.log('check: all green.')
