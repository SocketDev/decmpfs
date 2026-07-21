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
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const repoRoot = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
)

// Dep-0 output helpers: process streams (never `console`, never the lib logger)
// so both publish workflows can run this gate with only Node on PATH. The stream
// access lives inside a function, so it stays lazy (no module-eval capture).
function err(message: string): void {
  process.stderr.write(`${message}\n`)
}

function fail(
  what: string,
  where: string,
  saw: string,
  want: string,
  fix: string,
): never {
  err('✗ decmpfs version gate failed')
  err(`  What:  ${what}`)
  err(`  Where: ${where}`)
  err(`  Saw:   ${saw}`)
  err(`  Want:  ${want}`)
  err(`  Fix:   ${fix}`)
  process.exit(1)
}

function out(message: string): void {
  process.stdout.write(`${message}\n`)
}

const cargoPath = path.join(repoRoot, 'crates', 'decmpfs', 'Cargo.toml')
const cargo = readFileSync(cargoPath, 'utf8')
// The top-level `version = "…"` is the [package] version; dependency pins are
// nested inline tables (`zstd = { version = "…" }`), never at line-start.
const crateVersion = cargo.match(/^version\s*=\s*"(?<version>[^"]+)"/m)?.[1]
if (crateVersion === undefined) {
  fail(
    'crate version not found',
    cargoPath,
    'no top-level `version = "…"`',
    'a semver string under [package]',
    'add `version = "x.y.z"` to [package]',
  )
}

const pkgPath = path.join(repoRoot, 'napi', 'decmpfs', 'package.json')
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

const optDeps: Record<string, string> = pkg.optionalDependencies ?? {}
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

out(
  `✓ decmpfs versions in sync at ${crateVersion} ` +
    `(crate + npm + ${platformPins.length} platform packages)`,
)
