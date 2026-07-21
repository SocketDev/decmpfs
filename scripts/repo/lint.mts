// Minimal staged/modified-scoped lint runner (rustfmt), mirroring the
// socket-* CLI contract so git hooks and CI call the same entrypoints:
//
//   node scripts/repo/lint.mts            # modified .rs files (working tree vs HEAD)
//   node scripts/repo/lint.mts --staged   # staged .rs files (pre-commit)
//   node scripts/repo/lint.mts --all      # whole workspace (cargo fmt)
//   node scripts/repo/lint.mts --fix      # rewrite instead of --check
//
// Scope escalates to --all automatically when a config that affects every
// file is in scope (rustfmt.toml, Cargo.toml). No files in scope → no-op.

import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

const logger = getDefaultLogger()

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
const args = process.argv.slice(2)
const staged = args.includes('--staged')
const all = args.includes('--all')
const fix = args.includes('--fix')

function gitLines(gitArgs: string[]): string[] {
  const result = spawnSync('git', gitArgs, { cwd: root, encoding: 'utf8' })
  if (result.status !== 0) {
    return []
  }
  return String(result.stdout ?? '')
    .split('\n')
    .map(l => l.trim())
    .filter(Boolean)
}

function run(cmd: string, cmdArgs: string[]): void {
  const result = spawnSync(cmd, cmdArgs, { cwd: root, stdio: 'inherit' })
  if (result.status !== 0) {
    throw new Error(`${cmd} exited ${result.status ?? 'on a signal'}`)
  }
}

const ESCALATORS = new Set(['.rustfmt.toml', 'Cargo.toml', 'rustfmt.toml'])

const scoped = all
  ? []
  : gitLines(
      staged
        ? ['diff', '--cached', '--name-only', '--diff-filter=ACM']
        : ['diff', '--name-only', '--diff-filter=ACM', 'HEAD'],
    )
const escalate = all || scoped.some(f => ESCALATORS.has(path.basename(f)))
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
    logger.log(`No ${staged ? 'staged' : 'modified'} .rs files; skipping lint.`)
    process.exit(0)
  }
} catch {
  logger.fail(
    fix
      ? 'lint: rustfmt failed.'
      : 'lint: formatting issues found. Fix: node scripts/repo/lint.mts --fix' +
          (staged ? ' --staged' : ''),
  )
  process.exit(1)
}
logger.log('lint: clean.')
