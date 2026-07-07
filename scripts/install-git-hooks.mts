// Point git at the tracked .git-hooks/ directory. Runs from npm `prepare`
// (any `npm install` at the root) and is safe to run by hand:
//
//   node scripts/install-git-hooks.mts

import { execFileSync } from 'node:child_process'
import { chmodSync, readdirSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..')
const hooksDir = path.join(root, '.git-hooks')

try {
  execFileSync('git', ['config', 'core.hooksPath', '.git-hooks'], {
    cwd: root,
  })
} catch {
  // Not a git checkout (a published tarball, a CI cache restore) — nothing
  // to wire.
  process.exit(0)
}
for (const name of readdirSync(hooksDir)) {
  if (!name.includes('.')) {
    chmodSync(path.join(hooksDir, name), 0o755)
  }
}
console.log('git hooks: core.hooksPath -> .git-hooks')
