// Build the release cdylib and stage it as the loadable `decmpfs.node` addon.
// Platform-aware: cargo emits a different artifact name per OS (a `lib` prefix on
// Unix, none on Windows), so a hardcoded `.dylib` copy only works on macOS.

import { execFileSync } from 'node:child_process'
import { copyFileSync } from 'node:fs'
import { join } from 'node:path'

// The cdylib filename cargo writes to target/release for each platform. The Rust
// crate's lib artifact is `decmpfs_node`.
const ARTIFACT: Record<string, string | undefined> = {
  darwin: 'libdecmpfs_node.dylib',
  linux: 'libdecmpfs_node.so',
  win32: 'decmpfs_node.dll',
}

const artifact = ARTIFACT[process.platform]
if (!artifact) {
  throw new Error(
    `decmpfs build: no cdylib artifact mapping for platform "${process.platform}" — ` +
      `add it to node/scripts/build.mts (expected darwin, linux, or win32).`,
  )
}

execFileSync('cargo', ['build', '--release'], { stdio: 'inherit' })
copyFileSync(join('target', 'release', artifact), 'decmpfs.node')
