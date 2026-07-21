// Full static gate: format check + clippy (the Rust "typecheck") across the
// workspace. Pre-push and CI both run this so a push never lands what CI
// would reject.
//
//   node scripts/repo/check.mts [--node-only | --rust-only]

import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
// prefer-async-spawn: sync-required — this is a dep-0 CI gate (the ci.yml test
// job runs it with no install), so it cannot import the lib spawn; the flow is a
// sequence of synchronous gates.
import { spawnSync } from 'node:child_process'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
const nodeOnly = process.argv.includes('--node-only')
const rustOnly = process.argv.includes('--rust-only')

// Dep-0 output helpers: process streams (never `console`, never the lib logger)
// so this gate runs with only Node on PATH. The stream access sits inside a
// function, so it stays lazy (no module-eval handle capture).
function err(message: string): void {
  process.stderr.write(`${message}\n`)
}

function out(message: string): void {
  process.stdout.write(`${message}\n`)
}

function run(label: string, cmd: string, args: string[]): void {
  out(`check: ${label}`)
  const result = spawnSync(cmd, args, { cwd: root, stdio: 'inherit' })
  if (result.status !== 0) {
    err(`check: ${label} failed.`)
    process.exit(1)
  }
}

if (nodeOnly && rustOnly) {
  err('check: --node-only and --rust-only cannot be combined.')
  process.exit(1)
}

if (!nodeOnly) {
  run('cargo fmt --check', 'cargo', ['fmt', '--all', '--check'])

  // Clippy across the feature matrix. Default features never compile the
  // `addon` / `exe` modules, so their panic-free deny is only enforced when
  // those features are actually linted — run each.
  const featureSets = [
    { label: 'default', args: [] },
    { label: 'addon', args: ['--features', 'addon'] },
    { label: 'exe', args: ['--features', 'exe'] },
  ]
  for (const { label, args } of featureSets) {
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
  // Test the feature-gated code too — a green clippy doesn't run the tests,
  // and a default-feature `cargo test` never compiles these modules.
  run('cargo test (addon)', 'cargo', ['test', '--features', 'addon'])
  run('cargo test (exe)', 'cargo', ['test', '--features', 'exe'])
}
run('version parity', process.execPath, [
  path.join(root, 'scripts', 'repo', 'check-versions.mts'),
])
if (!rustOnly) {
  // Type-check the hand-maintained napi declarations with the pinned
  // TypeScript compiler.
  run(
    'tsc --noEmit (napi type declarations)',
    path.join(root, 'node_modules', '.bin', 'tsc'),
    [
      '--noEmit',
      '--project',
      path.join(root, 'napi', 'decmpfs', 'tsconfig.json'),
    ],
  )
  // Brand-asset guard: render the "for X" lockups and assert the label stays
  // small relative to the wordmark.
  run('asset render test (for-label sizing)', 'node', [
    '--test',
    path.join(root, 'scripts', 'repo', 'gen', 'logo.test.mts'),
  ])
  // The traced wordmark SVGs must stay svgo-optimized.
  run('svg optimized (drift)', process.execPath, [
    path.join(root, 'scripts', 'repo', 'gen', 'optimize-svg.mts'),
    '--check',
  ])
}
out('check: all green.')
