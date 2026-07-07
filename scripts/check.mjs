// Full static gate: format check + clippy (the Rust "typecheck") across the
// workspace. Pre-push and CI both run this so a push never lands what CI
// would reject.
//
//   node scripts/check.mjs

import { execFileSync } from 'node:child_process'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..')

function run(label, cmd, args) {
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
// Test the feature-gated code too — a green clippy doesn't run the tests.
run('cargo test (exe)', 'cargo', ['test', '--features', 'exe'])
run('version parity', process.execPath, [
  path.join(root, 'scripts', 'check-versions.mjs'),
])
console.log('check: all green.')
