// Point git at the tracked .git-hooks/ directory. Runs from pnpm `prepare`
// (any `pnpm install` at the root) and is safe to run by hand:
//
//   node scripts/repo/install-git-hooks.mts

import { chmodSync, readdirSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

const logger = getDefaultLogger()

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
const hooksDir = path.join(root, '.git-hooks')

const configResult = spawnSync(
  'git',
  ['config', 'core.hooksPath', '.git-hooks'],
  { cwd: root },
)
if (configResult.status !== 0) {
  // Not a git checkout (a published tarball, a CI cache restore) — nothing
  // to wire.
  process.exit(0)
}
for (const name of readdirSync(hooksDir)) {
  if (!name.includes('.')) {
    chmodSync(path.join(hooksDir, name), 0o755)
  }
}
logger.log('git hooks: core.hooksPath -> .git-hooks')
