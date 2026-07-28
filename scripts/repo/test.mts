// Scope-aware test runner, mirroring the socket-* CLI contract so git hooks and
// CI call the same entrypoint:
//
//   node scripts/repo/test.mts            # test if modified .rs (working tree vs HEAD)
//   node scripts/repo/test.mts --staged   # pre-commit: test if staged .rs; else no-op
//   node scripts/repo/test.mts --all      # whole workspace via cargo test
//   node scripts/repo/test.mts <t.test.mts…>  # run the named JS tests (node --test)
//
// The crate is the product, so `cargo test --workspace --locked` is the suite
// (matching CI's "Test crate" step). The staged/modified scopes skip when no
// Rust is in scope so the pre-commit hook stays fast — the full suite runs under
// --all (pre-push + CI). Dep-0: node strips the .mts types and the flow uses
// only node builtins, so git hooks run it with nothing installed.

// prefer-async-spawn: sync-required — dep-0 test runner (git hooks invoke it
// with only Node on PATH), so it cannot import the lib spawn; the whole flow is
// a single synchronous cargo invocation.
import { spawnSync } from 'node:child_process'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
const args = process.argv.slice(2)
const staged = args.includes('--staged')
const all = args.includes('--all')

// Explicit JS test files name exactly what to run — never Rust-gated. CI's
// bare `node --test` discovers the same files; this keeps the local wrapper
// honest instead of silently skipping a named test.
const jsTests = args.filter(arg => /\.test\.[cm]?[jt]s$/.test(arg))

// A config change that affects every test escalates to the whole suite.
const ESCALATORS = new Set([
  '.rustfmt.toml',
  'Cargo.lock',
  'Cargo.toml',
  'rustfmt.toml',
])

function gitLines(gitArgs: string[]): string[] {
  const result = spawnSync('git', gitArgs, { cwd: root, encoding: 'utf8' })
  if (result.status !== 0) {
    return []
  }
  return (result.stdout ?? '')
    .split('\n')
    .map(line => line.trim())
    .filter(Boolean)
}

function out(message: string): void {
  process.stdout.write(`${message}\n`)
}

if (jsTests.length > 0) {
  const jsResult = spawnSync(process.execPath, ['--test', ...jsTests], {
    cwd: root,
    stdio: 'inherit',
  })
  process.exitCode = jsResult.status ?? 1
} else {
  let rustInScope = true
  if (!all) {
    const scoped = gitLines(
      staged
        ? ['diff', '--cached', '--name-only', '--diff-filter=ACM']
        : ['diff', '--name-only', '--diff-filter=ACM', 'HEAD'],
    )
    rustInScope = scoped.some(
      file => file.endsWith('.rs') || ESCALATORS.has(path.basename(file)),
    )
    if (!rustInScope) {
      out(`No ${staged ? 'staged' : 'modified'} Rust changes; skipping tests.`)
    }
  }
  if (rustInScope) {
    const result = spawnSync('cargo', ['test', '--workspace', '--locked'], {
      cwd: root,
      stdio: 'inherit',
    })
    process.exitCode = result.status ?? 1
  }
}
