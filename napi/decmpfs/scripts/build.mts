// Build the release cdylib and stage it as the loadable `decmpfs.node` addon.
// Platform-aware: cargo emits a different artifact name per OS (a `lib` prefix on
// Unix, none on Windows), so a hardcoded `.dylib` copy only works on macOS.

// prefer-async-spawn: sync-required — this is a dep-0 napi build script (CI runs
// it with no node_modules), so it cannot import the lib spawn; the whole flow is
// a single synchronous cargo build.
import { spawnSync } from 'node:child_process'
import { copyFileSync } from 'node:fs'
import * as path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

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
      `add it to napi/decmpfs/scripts/build.mts (expected darwin, linux, or win32).`,
  )
}

// This package is a member of the cargo workspace rooted at the repo, so cargo
// writes the cdylib to the WORKSPACE-ROOT target/, not this package's dir.
const nodeRoot = path.join(path.dirname(fileURLToPath(import.meta.url)), '..')
const repoRoot = path.join(nodeRoot, '..', '..')

// A cross build (--target <triple>) lands the cdylib under target/<triple>/release;
// a native build lands it in target/release directly.
const targetIndex = process.argv.indexOf('--target')
const target = targetIndex === -1 ? undefined : process.argv[targetIndex + 1]
if (targetIndex !== -1 && !target) {
  throw new Error('decmpfs build: --target needs a triple value.')
}

const cargoArgs = ['build', '-p', 'decmpfs-node', '--release']
if (target) cargoArgs.push('--target', target)
const build = spawnSync('cargo', cargoArgs, {
  cwd: repoRoot,
  stdio: 'inherit',
})
if (build.status !== 0) {
  throw new Error(
    `decmpfs build: cargo build exited ${build.status ?? 'on a signal'}.`,
  )
}
const builtDir = target
  ? path.join(repoRoot, 'target', target, 'release')
  : path.join(repoRoot, 'target', 'release')
copyFileSync(
  path.join(builtDir, artifact),
  path.join(nodeRoot, 'decmpfs.node'),
)
