// Minimal staged/modified-scoped lint runner (rustfmt), mirroring the
// socket-* CLI contract so git hooks and CI call the same entrypoints:
//
//   node scripts/lint.mts            # modified .rs files (working tree vs HEAD)
//   node scripts/lint.mts --staged   # staged .rs files (pre-commit)
//   node scripts/lint.mts --all      # whole workspace (cargo fmt)
//   node scripts/lint.mts --fix      # rewrite instead of --check
//
// Scope escalates to --all automatically when a config that affects every
// file is in scope (rustfmt.toml, Cargo.toml). No files in scope → no-op.

import { execFileSync } from 'node:child_process'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..')
const args = process.argv.slice(2)
const staged = args.includes('--staged')
const all = args.includes('--all')
const fix = args.includes('--fix')

function gitLines(gitArgs: string[]): string[] {
  try {
    return String(execFileSync('git', gitArgs, { cwd: root, encoding: 'utf8' }))
      .split('\n')
      .map(l => l.trim())
      .filter(Boolean)
  } catch {
    return []
  }
}

function run(cmd: string, cmdArgs: string[]): void {
  execFileSync(cmd, cmdArgs, { cwd: root, stdio: 'inherit' })
}

const ESCALATORS = new Set(['Cargo.toml', 'rustfmt.toml', '.rustfmt.toml'])

const scoped = all
  ? []
  : gitLines(
      staged
        ? ['diff', '--cached', '--name-only', '--diff-filter=ACM']
        : ['diff', '--name-only', '--diff-filter=ACM', 'HEAD'],
    )
const escalate =
  all || scoped.some(f => ESCALATORS.has(path.basename(f)))
const rsFiles = scoped.filter(f => f.endsWith('.rs'))

try {
  if (escalate) {
    run('cargo', ['fmt', '--all', ...(fix ? [] : ['--check'])])
  } else if (rsFiles.length) {
    run('rustfmt', [
      '--edition',
      '2021',
      ...(fix ? [] : ['--check']),
      ...rsFiles,
    ])
  } else {
    console.log(`No ${staged ? 'staged' : 'modified'} .rs files; skipping lint.`)
    process.exit(0)
  }
} catch {
  console.error(
    fix
      ? 'lint: rustfmt failed.'
      : 'lint: formatting issues found. Fix: node scripts/lint.mts --fix' +
          (staged ? ' --staged' : ''),
  )
  process.exit(1)
}
console.log('lint: clean.')
