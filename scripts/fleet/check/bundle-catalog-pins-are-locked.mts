#!/usr/bin/env node
/*
 * @file `check --all` gate: the COMMITTED `pnpm-lock.yaml` satisfies the
 *   catalog the repo's `pnpm-workspace.yaml` carries ON DISK. For a thin
 *   member those are two different generations of the same file — the bundle's
 *   installer merges the fleet-managed workspace keys (`catalog:`,
 *   `overrides:`, the release-age settings — see
 *   scripts/repo/release-bundle/workspace-segment.mts) into the working tree at
 *   hydrate time, entry-scoped, with every entry the bundle ships taking the
 *   BUNDLE's text. So the on-disk catalog is the bundle's, while the lockfile
 *   beside it in git is whatever the member last committed. When those two
 *   disagree, the next `pnpm install --frozen-lockfile` dies, and it dies in
 *   one of two shapes that look unrelated and have one cause:
 *
 *   - `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH` naming `catalogs` — the drifted pin
 *     already had a resolved entry, so pnpm sees the recorded specifier and
 *     the workspace value disagree. Reported as `specifier-drift`.
 *   - `pnpm-lock.yaml has no integrity for <pkg>@<ver>` — a version moved and
 *     the lockfile's `catalogs:` block resolves one its `packages:` section
 *     never gained a resolution for. Reported as `unresolved-version`.
 *
 *   That is the defect that broke six repos on the thin conversion: each one's
 *   committed lockfile predated the catalog the bundle hydrates, and nothing
 *   said so until CI installed. Three of the four known-bad commits are
 *   INVISIBLE against the committed workspace file alone — the drift only
 *   exists once the bundle has merged — which is why this reads the working
 *   tree for the catalog and git for the lockfile, not one source for both.
 *
 *   Sources, and why each side comes from where it does:
 *
 *   - catalog — the WORKING TREE's `pnpm-workspace.yaml`. That is the file
 *     pnpm itself will read, bundle merge already applied. Reading the
 *     committed copy would audit the generation the bundle is about to
 *     replace.
 *   - lockfile — the COMMITTED `pnpm-lock.yaml` (`git show HEAD:`), falling
 *     back to the working copy when git cannot answer. A non-frozen local
 *     `pnpm install` silently REWRITES the working lockfile to match the new
 *     catalog, so the working copy always looks fine; the staleness only
 *     exists in git, and CI is the first thing to read it. Auditing the disk
 *     copy would pass on exactly the tree that breaks CI.
 *
 *   NOT claimed: this cannot see a bundle the repo has not hydrated yet. It
 *   audits the catalog currently on disk, so a member that has not run its
 *   fetch since the last release is checked against the previous bundle. The
 *   fetch runs in `prepare` and in every CI setup step, so the hydrated tree
 *   is the normal state; a stale one under-reports rather than false-fires.
 *
 *   Vacuous pass when either file is absent (a repo with no workspace catalog
 *   or no install). Exit: 0 — clean / nothing to audit; 1 — the committed
 *   lockfile does not satisfy the on-disk catalog.
 *
 *   Usage: node scripts/fleet/check/bundle-catalog-pins-are-locked.mts [--quiet]
 */

import { existsSync, readFileSync } from 'node:fs'
import process from 'node:process'

import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

import {
  parseCatalogBlock,
  parseNamedCatalogs,
} from '../lib/workspace-yaml.mts'
import { PNPM_LOCK, PNPM_WORKSPACE_YAML, REPO_ROOT } from '../paths.mts'
import { isMainModule } from '../_shared/is-main-module.mts'
import { parsePnpmLockfileText } from '../_shared/pnpm-lockfile.mts'
import { runMain } from '../_shared/run-main.mts'

import type { PnpmLockfileCatalogEntry } from '../_shared/pnpm-lockfile.mts'
import type { ScriptMeta } from '../_shared/run-main.mts'

const logger = getDefaultLogger()

// The lockfile's own name for the unnamed catalog block.
const DEFAULT_CATALOG = 'default'

/**
 * Which install failure a finding will become. Both are the same drift; they
 * differ only in whether the lockfile had already recorded a resolution for
 * the pin that moved, which is what decides the error pnpm prints.
 */
export type CatalogLockFindingKind = 'specifier-drift' | 'unresolved-version'

export interface CatalogLockFinding {
  // The catalog the entry belongs to — `default` for the unnamed block.
  readonly catalog: string
  readonly kind: CatalogLockFindingKind
  // For `specifier-drift`, the specifier the lockfile recorded. For
  // `unresolved-version`, the `<pkg>@<ver>` dep path it has no resolution for.
  readonly locked: string
  // The catalog entry's name, which for an alias is NOT the resolved package.
  readonly name: string
  // The value the on-disk catalog pins, empty for `unresolved-version` (that
  // arm reads only the lockfile).
  readonly wanted: string
}

/**
 * The package + version a catalog SPECIFIER resolves against, given the entry
 * name and the version pnpm recorded. A `-stable` alias
 * (`npm:@socketsecurity/lib@6.7.0`) resolves against its TARGET's name, not
 * the entry's, so the resolution lookup has to follow the alias — checking
 * `@socketsecurity/lib-stable@6.7.0` would report every alias as unresolved.
 */
export function catalogEntryDepPath(entry: PnpmLockfileCatalogEntry): string {
  if (entry.specifier.startsWith('npm:')) {
    const target = entry.specifier.slice('npm:'.length)
    const at = target.lastIndexOf('@')
    // `npm:@scope/pkg` with no version pins whatever the range resolved to;
    // an `@` at index 0 is the scope sigil, not a version separator.
    if (at > 0) {
      return `${target.slice(0, at)}@${entry.version}`
    }
    return `${target}@${entry.version}`
  }
  return `${entry.name}@${entry.version}`
}

/**
 * Audit one repo's on-disk catalog against its committed lockfile. Pure — both
 * sides arrive as text, so the two failure shapes are unit-testable without a
 * tree. Findings are sorted by catalog then name for stable output.
 */
export function auditCatalogAgainstLockfile(config: {
  lockfileYaml: string
  workspaceYaml: string
}): CatalogLockFinding[] {
  const cfg = Object.assign(Object.create(null), config) as typeof config
  const graph = parsePnpmLockfileText(cfg.lockfileYaml)
  const findings: CatalogLockFinding[] = []

  // The workspace catalogs, keyed the way the lockfile keys them.
  const workspaceCatalogs = new Map<string, Record<string, string>>([
    [DEFAULT_CATALOG, parseCatalogBlock(cfg.workspaceYaml)],
  ])
  const named = parseNamedCatalogs(cfg.workspaceYaml)
  const namedKeys = Object.keys(named)
  for (let i = 0, { length } = namedKeys; i < length; i += 1) {
    workspaceCatalogs.set(namedKeys[i]!, named[namedKeys[i]!]!)
  }

  const { catalogEntries } = graph
  for (let i = 0, { length } = catalogEntries; i < length; i += 1) {
    const entry = catalogEntries[i]!
    const wanted = workspaceCatalogs.get(entry.catalog)?.[entry.name]
    // An entry the lockfile records but the on-disk catalog no longer declares
    // is a RETIRED pin, not drift: pnpm drops it on the next write and no
    // importer can still reference it (a `catalog:` ref with no entry fails
    // earlier, with ERR_PNPM_CATALOG_ENTRY_NOT_FOUND_FOR_SPEC, and
    // baseline-catalog-deps-are-covered is the gate for that). A `catalog:`
    // forward has no concrete value to compare either.
    if (wanted !== undefined && !wanted.startsWith('catalog:')) {
      if (wanted !== entry.specifier) {
        findings.push({
          catalog: entry.catalog,
          kind: 'specifier-drift',
          locked: entry.specifier,
          name: entry.name,
          wanted,
        })
        // The resolution arm below would fire on the same entry for the same
        // cause; one finding per drifted pin keeps the report readable.
        continue
      }
    }
    // Resolution arm: the lockfile agrees with the catalog (or the catalog is
    // silent) but its own `packages:`/`snapshots:` sections never gained a
    // resolution for the version it recorded. That is a lockfile edited in
    // place — the catalogs block moved, the resolutions did not — and pnpm
    // reports it as a missing integrity rather than a config mismatch.
    const depPath = catalogEntryDepPath(entry)
    const at = depPath.lastIndexOf('@')
    const resolvedName = depPath.slice(0, at)
    const resolvedVersion = depPath.slice(at + 1)
    if (!graph.versionsByName.get(resolvedName)?.has(resolvedVersion)) {
      findings.push({
        catalog: entry.catalog,
        kind: 'unresolved-version',
        locked: depPath,
        name: entry.name,
        wanted: '',
      })
    }
  }

  return findings.toSorted(
    (a, b) =>
      a.catalog.localeCompare(b.catalog) || a.name.localeCompare(b.name),
  )
}

export interface LockfileSource {
  readonly origin: 'committed' | 'working-tree'
  readonly text: string
}

/**
 * The COMMITTED lockfile text, or the working copy when git cannot answer —
 * no repository, no HEAD because the repo carries no commits yet, or the path
 * not tracked at HEAD. The origin rides along so the failure text can name
 * which generation
 * it audited; a working-tree read is weaker, and saying so beats implying a
 * committed-state verdict the check did not make.
 */
export function readCommittedLockfile(
  lockfilePath: string,
  repoRoot: string,
): LockfileSource | undefined {
  const res = spawnSync('git', ['show', 'HEAD:pnpm-lock.yaml'], {
    cwd: repoRoot,
    encoding: 'utf8',
  })
  if (res.status === 0 && typeof res.stdout === 'string' && res.stdout) {
    return { origin: 'committed', text: res.stdout }
  }
  if (!existsSync(lockfilePath)) {
    return undefined
  }
  return { origin: 'working-tree', text: readFileSync(lockfilePath, 'utf8') }
}

export async function main(): Promise<void> {
  const quiet = process.argv.includes('--quiet')
  if (!existsSync(PNPM_WORKSPACE_YAML)) {
    if (!quiet) {
      logger.log(
        'bundle-catalog-pins-are-locked: no pnpm-workspace.yaml — vacuous pass.',
      )
    }
    process.exitCode = 0
    return
  }
  const lockfile = readCommittedLockfile(PNPM_LOCK, REPO_ROOT)
  if (lockfile === undefined) {
    if (!quiet) {
      logger.log(
        'bundle-catalog-pins-are-locked: no pnpm-lock.yaml — vacuous pass.',
      )
    }
    process.exitCode = 0
    return
  }

  const findings = auditCatalogAgainstLockfile({
    lockfileYaml: lockfile.text,
    workspaceYaml: readFileSync(PNPM_WORKSPACE_YAML, 'utf8'),
  })

  if (findings.length === 0) {
    if (!quiet) {
      logger.log(
        `bundle-catalog-pins-are-locked: the ${lockfile.origin} pnpm-lock.yaml satisfies the on-disk catalog.`,
      )
    }
    process.exitCode = 0
    return
  }

  logger.fail(
    `bundle-catalog-pins-are-locked: ${findings.length} catalog pin(s) the ${lockfile.origin} pnpm-lock.yaml does not satisfy:`,
  )
  for (let i = 0, { length } = findings; i < length; i += 1) {
    const finding = findings[i]!
    const where =
      finding.catalog === DEFAULT_CATALOG
        ? finding.name
        : `${finding.catalog}.${finding.name}`
    logger.fail(
      finding.kind === 'specifier-drift'
        ? `  ${where}: catalog pins ${finding.wanted}, lockfile resolved ${finding.locked}`
        : `  ${where}: lockfile resolves ${finding.locked} with no resolution entry for it`,
    )
  }
  logger.fail(
    '  What:   the catalog on disk — the bundle merges its fleet-managed\n' +
      '          workspace keys into pnpm-workspace.yaml at hydrate time — moved\n' +
      '          past what the committed pnpm-lock.yaml records.\n' +
      '  Where:  pnpm-workspace.yaml (working tree) vs pnpm-lock.yaml (HEAD).\n' +
      '  Wanted: every catalog entry the lockfile records carries the catalog\n' +
      '          value on disk, and a resolution for the version it names.\n' +
      '  Saw:    a `pnpm install --frozen-lockfile` that dies with\n' +
      '          ERR_PNPM_LOCKFILE_CONFIG_MISMATCH (catalogs) on a drifted\n' +
      '          specifier, or `no integrity for <pkg>@<ver>` on a moved one.\n' +
      '  Fix:    run `pnpm install` and COMMIT the regenerated pnpm-lock.yaml.\n' +
      '          A local install rewrites it silently, so the tree can look\n' +
      '          clean while the commit that lands is the one CI rejects.',
  )
  process.exitCode = 1
}

/* c8 ignore start - entrypoint guard; exercised via subprocess */
const SCRIPT_META: ScriptMeta = {
  describe:
    'checks the committed pnpm-lock.yaml satisfies the catalog the bundle hydrates onto disk',
  help: `Usage: node scripts/fleet/check/bundle-catalog-pins-are-locked.mts [flags]
  --quiet  suppress the success message`,
}

if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
/* c8 ignore stop */
