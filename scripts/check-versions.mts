/**
 * Assert the decmpfs crate and every npm package agree on one version.
 *
 * The core crate (crates/decmpfs/Cargo.toml) ships to crates.io and the addon
 * (napi/decmpfs/package.json) plus its @decmpfs/<triple> platform packages ship
 * to npm; they release in lockstep, so a version mismatch is a release-blocking
 * defect. Both publish workflows run this gate first. Node 24 (the repo
 * baseline, .node-version) strips the .mts types natively.
 */

import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..')

function fail(
  what: string,
  where: string,
  saw: string,
  want: string,
  fix: string,
): never {
  console.error('✗ decmpfs version gate failed')
  console.error(`  What:  ${what}`)
  console.error(`  Where: ${where}`)
  console.error(`  Saw:   ${saw}`)
  console.error(`  Want:  ${want}`)
  console.error(`  Fix:   ${fix}`)
  process.exit(1)
}

const cargoPath = join(repoRoot, 'crates', 'decmpfs', 'Cargo.toml')
const cargo = readFileSync(cargoPath, 'utf8')
// The top-level `version = "…"` is the [package] version; dependency pins are
// nested inline tables (`zstd = { version = "…" }`), never at line-start.
const crateMatch = cargo.match(/^version\s*=\s*"([^"]+)"/m)
if (!crateMatch) {
  fail(
    'crate version not found',
    cargoPath,
    'no top-level `version = "…"`',
    'a semver string under [package]',
    'add `version = "x.y.z"` to [package]',
  )
}
const crateVersion = crateMatch[1]

const pkgPath = join(repoRoot, 'napi', 'decmpfs', 'package.json')
const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'))

if (pkg.version !== crateVersion) {
  fail(
    'crate and npm package versions disagree',
    `${cargoPath} vs ${pkgPath}`,
    `crate ${crateVersion}, npm ${pkg.version}`,
    'identical versions',
    'set both to the same version before publishing',
  )
}

const optDeps = pkg.optionalDependencies ?? {}
const platformPins = Object.entries(optDeps).filter(([name]) =>
  name.startsWith('@decmpfs/'),
)
for (const [name, range] of platformPins) {
  if (range !== crateVersion) {
    fail(
      'platform package pin disagrees with the release version',
      `${pkgPath} optionalDependencies["${name}"]`,
      range,
      crateVersion,
      `set "${name}" to "${crateVersion}"`,
    )
  }
}

console.log(
  `✓ decmpfs versions in sync at ${crateVersion} ` +
    `(crate + npm + ${platformPins.length} platform packages)`,
)
