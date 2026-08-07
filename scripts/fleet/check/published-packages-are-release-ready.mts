#!/usr/bin/env node
/*
 * @file `check --all` gate: every package a repo DECLARES it publishes is
 *   actually publishable, and the whole declared set carries ONE version.
 *
 *   The declaration lives in `.config/repo/socket-wheelhouse.json` under
 *   `release.publishedPackages`. It is the single source of truth for "these
 *   ship", and the release pipeline reads it. Two ways a manifest can silently
 *   contradict it:
 *
 *   1. `"private": true`. npm refuses to publish a private package, so the
 *      release simply skips it. The run stays GREEN — nothing failed, the
 *      package just never went out. A consumer installing the loader then gets
 *      an optional platform dependency that does not exist.
 *
 *   2. Version drift across the set. Variants of one product (a `.rs`, its
 *      `.wasm`, and every `.exe-<platform>` / `.node-<platform>`) are installed
 *      TOGETHER: the loader pins its platform siblings by exact version. If the
 *      set does not move as one, the loader pins a sibling version that was
 *      never published, and the install breaks for exactly the platforms whose
 *      package lagged.
 *
 *   Both were live in ultrathink when this check was written: 18 packages
 *   declared, 17 of them `private: true`, versions split across `0.0.0` and
 *   `0.1.1`. The config said "publish these" for two years of commits while the
 *   manifests said "we cannot".
 *
 *   Scope: TRACKED manifests only (`git ls-files`), so generated build output
 *   like wasm-pack's `pkg-node/` is never mistaken for a declared package.
 *
 *   `--fix` moves FORWARD, never backward: it drops `private` from a declared
 *   package and raises every version in the set to the highest one present. It
 *   never lowers a version, so an already-published version is never reused.
 *
 *   Vacuous pass when the repo declares no published packages.
 *
 *   Exit: 0 — every declared package is publishable and synced; 1 — a declared
 *   package is private, or the set carries more than one version.
 *
 *   Usage: node scripts/fleet/check/published-packages-are-release-ready.mts [--fix] [--quiet]
 */

import { readFileSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'

import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'
import { compare } from '@socketsecurity/lib-stable/versions/compare'

import { REPO_ROOT } from '../paths.mts'
import { isMainModule } from '../_shared/is-main-module.mts'
import { runMain } from '../_shared/run-main.mts'

import type { ScriptMeta } from '../_shared/run-main.mts'

const logger = getDefaultLogger()

const REPO_CONFIG_REL = '.config/repo/socket-wheelhouse.json'

export interface DeclaredPackage {
  manifestPath: string
  name: string
  private: boolean
  version: string
}

/**
 * The package names a repo declares it publishes, from
 * `release.publishedPackages`. Empty when the config is absent or declares
 * none — the vacuous-pass case.
 */
export function readDeclaredPublishes(repoRoot: string): string[] {
  try {
    const raw = readFileSync(path.join(repoRoot, REPO_CONFIG_REL), 'utf8')
    const cfg = JSON.parse(raw) as {
      release?: { publishedPackages?: unknown | undefined } | undefined
    }
    const declared = cfg.release?.publishedPackages
    if (!Array.isArray(declared)) {
      return []
    }
    return declared.filter((n): n is string => typeof n === 'string')
  } catch {
    return []
  }
}

/**
 * Every TRACKED package.json in the repo, keyed by package name. Tracked-only
 * is load-bearing: build output (wasm-pack's `pkg-node/`, a napi staging dir)
 * carries a package.json with a real name, and treating one as a declared
 * package would compare a generated artifact's version against the source's.
 */
export function readTrackedManifests(
  repoRoot: string,
): Map<string, DeclaredPackage> {
  const byName = new Map<string, DeclaredPackage>()
  const result = spawnSync('git', ['ls-files', '*package.json'], {
    cwd: repoRoot,
    stdio: 'pipe',
    stdioString: true,
  })
  if (result.status !== 0) {
    return byName
  }
  const files = String(result.stdout ?? '')
    .replace(/\r\n/g, '\n')
    .split('\n')
    .map(s => s.trim())
    .filter(Boolean)
  for (let i = 0, { length } = files; i < length; i += 1) {
    const rel = files[i]!
    try {
      const manifest = JSON.parse(
        readFileSync(path.join(repoRoot, rel), 'utf8'),
      ) as {
        name?: unknown | undefined
        private?: unknown | undefined
        version?: unknown | undefined
      }
      if (typeof manifest.name !== 'string') {
        continue
      }
      byName.set(manifest.name, {
        manifestPath: rel,
        name: manifest.name,
        private: manifest.private === true,
        version:
          typeof manifest.version === 'string' ? manifest.version : '0.0.0',
      })
    } catch {
      // An unparseable manifest is another check's finding, not this one's.
    }
  }
  return byName
}

/**
 * The highest version in `versions` by semver order. Used as the sync target so
 * `--fix` only ever moves a package FORWARD — raising a lagging variant to the
 * set's leader can never reuse a version the registry already has.
 */
export function highestVersion(versions: readonly string[]): string {
  let best = versions[0] ?? '0.0.0'
  for (let i = 1, { length } = versions; i < length; i += 1) {
    const candidate = versions[i]!
    try {
      // `compare` answers undefined for an unparseable version rather than
      // throwing, so that case is handled here, not by the catch below.
      const order = compare(candidate, best)
      if (order !== undefined && order > 0) {
        best = candidate
      }
    } catch {
      // An unparseable version never wins, so a malformed entry cannot become
      // the target every other package is raised to.
    }
  }
  return best
}

export interface Findings {
  declared: DeclaredPackage[]
  missing: string[]
  privateOnes: DeclaredPackage[]
  versions: string[]
}

/**
 * Resolve the declared names against the tracked manifests and collect both
 * violations. Pure over its inputs; exported for tests.
 */
export function collectFindings(
  declaredNames: readonly string[],
  byName: ReadonlyMap<string, DeclaredPackage>,
): Findings {
  const declared: DeclaredPackage[] = []
  const missing: string[] = []
  const privateOnes: DeclaredPackage[] = []
  const versions = new Set<string>()
  for (let i = 0, { length } = declaredNames; i < length; i += 1) {
    const name = declaredNames[i]!
    const entry = byName.get(name)
    if (!entry) {
      missing.push(name)
      continue
    }
    declared.push(entry)
    if (entry.private) {
      privateOnes.push(entry)
    }
    versions.add(entry.version)
  }
  return { declared, missing, privateOnes, versions: [...versions].toSorted() }
}

/**
 * Apply the forward-only repair to one manifest: drop `private`, raise
 * `version` to `target`. Returns true when the file changed. Rewrites only
 * those two fields so key order and formatting elsewhere survive.
 */
export function applyFix(
  repoRoot: string,
  entry: DeclaredPackage,
  target: string,
): boolean {
  const abs = path.join(repoRoot, entry.manifestPath)
  const before = readFileSync(abs, 'utf8')
  const manifest = JSON.parse(before) as Record<string, unknown>
  let changed = false
  if (manifest['private'] === true) {
    delete manifest['private']
    changed = true
  }
  if (manifest['version'] !== target) {
    manifest['version'] = target
    changed = true
  }
  if (!changed) {
    return false
  }
  // Preserve the trailing newline convention npm writes.
  writeFileSync(abs, `${JSON.stringify(manifest, undefined, 2)}\n`)
  return true
}

export function main(): number {
  const fix = process.argv.includes('--fix')
  const quiet = process.argv.includes('--quiet')
  const declaredNames = readDeclaredPublishes(REPO_ROOT)
  if (!declaredNames.length) {
    if (!quiet) {
      logger.log(
        '[published-packages-are-release-ready] no release.publishedPackages declared here (not applicable).',
      )
    }
    return 0
  }

  const byName = readTrackedManifests(REPO_ROOT)
  const { declared, missing, privateOnes, versions } = collectFindings(
    declaredNames,
    byName,
  )
  const target = highestVersion(versions)
  const drifted = versions.length > 1

  if (fix && (privateOnes.length > 0 || drifted)) {
    let fixed = 0
    for (let i = 0, { length } = declared; i < length; i += 1) {
      if (applyFix(REPO_ROOT, declared[i]!, target)) {
        fixed += 1
      }
    }
    logger.success(
      `[published-packages-are-release-ready] repaired ${fixed} manifest(s): dropped \`private\`, synced to ${target}.`,
    )
    return 0
  }

  if (missing.length === 0 && privateOnes.length === 0 && !drifted) {
    if (!quiet) {
      logger.success(
        `[published-packages-are-release-ready] ${declared.length} declared package(s), all publishable and synced at ${target}.`,
      )
    }
    return 0
  }

  logger.fail(
    '[published-packages-are-release-ready] the declared publish set is not release-ready:',
  )
  if (privateOnes.length > 0) {
    logger.error(
      `  What:   ${privateOnes.length} of ${declared.length} declared package(s) are \`"private": true\`, so npm refuses to publish them and the release SKIPS them while staying green.`,
    )
    for (let i = 0, { length } = privateOnes; i < length; i += 1) {
      logger.substep(
        `${privateOnes[i]!.name} (${privateOnes[i]!.manifestPath})`,
      )
    }
  }
  if (drifted) {
    logger.error(
      `  What:   the set carries ${versions.length} different versions (${versions.join(', ')}). A loader pins its platform siblings by EXACT version, so an unsynced set pins a version that was never published.`,
    )
  }
  if (missing.length > 0) {
    logger.error(
      `  What:   ${missing.length} declared package(s) have no tracked manifest: ${missing.join(', ')}.`,
    )
  }
  logger.error(`  Where:  ${REPO_CONFIG_REL} — release.publishedPackages.`)
  logger.error(
    `  Saw:    ${declared.length} declared, ${privateOnes.length} private, version(s) ${versions.join(', ') || 'none'}.`,
  )
  logger.error(
    `  Fix:    run this check with --fix. It drops \`private\` and raises every declared package to ${target}, the highest already present — forward only, so a published version is never reused. If a package should NOT ship, remove it from release.publishedPackages instead.`,
  )
  return 1
}

const SCRIPT_META: ScriptMeta = {
  describe:
    'every declared published package is non-private and the set shares one version',
  help: `Usage: node scripts/fleet/check/published-packages-are-release-ready.mts [--fix] [--quiet]`,
}

/* c8 ignore start - entry-point wiring, exercised by running the script. */
if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
/* c8 ignore stop */
