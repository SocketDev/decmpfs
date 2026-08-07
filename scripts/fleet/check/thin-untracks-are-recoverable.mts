#!/usr/bin/env node
/*
 * @file `check --all` gate: every path a thin member untracks can actually be
 *   restored. Sibling to fleet-pack-ci-files-are-tracked.mts, and deliberately the
 *   opposite shape. That check is SYNTHETIC — it feeds fleetPackOwnedPaths a
 *   fabricated manifest built from the very arrays it guards, so a surface
 *   nobody thought to list is invisible to it. This one reads the repo's LIVE
 *   state: the per-file entries inside the `.gitignore` fleet block (exactly
 *   what the conversion untracked) against the applied-files marker (exactly
 *   what the last bundle placed). Neither input is authored by this check, so
 *   it can fail on a path no array mentions.
 *
 *   Two findings, both fatal:
 *
 *   - unrecoverable — a path is ignored but the bundle does NOT ship it. This
 *     is the "untracking blind deletes real content" trap: nothing restores
 *     it on a fresh clone, so the work is simply gone. Name-independent, which
 *     is the point.
 *   - must_stay_tracked — a path is ignored but isAlwaysTrackedSurface says a
 *     consumer reads it before our fetch runs. Defence in depth:
 *     fleetPackOwnedPaths already excludes these, so a hit here means the
 *     shipped fetcher regressed or a member hand-edited its gitignore block.
 *
 *   NOT claimed: this cannot tell a `.npmrc` (must stay tracked) from a
 *   `.editorconfig` (fine to untrack). Both ship in the bundle, so no
 *   structural property separates them — that distinction is semantic ("read
 *   before our fetch runs") and only the curated list in
 *   _shared/github-tracked-surface.mts can express it.
 *
 *   Vacuous pass on a fat member (no fleet gitignore block) or one that has
 *   never hydrated (no applied-files marker). Exit: 0 — clean / not thin;
 *   1 — an untracked path is unrecoverable or must stay tracked.
 *
 *   Usage: node scripts/fleet/check/thin-untracks-are-recoverable.mts [--quiet]
 */

import { existsSync, readFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'

import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { normalizePath } from '@socketsecurity/lib-stable/paths/normalize'

import { REPO_ROOT } from '../paths.mts'
import { findFleetRegions } from '../../../.claude/hooks/fleet/_shared/fleet-markers.mts'
import { isAlwaysTrackedSurface } from '../_shared/github-tracked-surface.mts'
import { isMainModule } from '../_shared/is-main-module.mts'
import { runMain } from '../_shared/run-main.mts'

import type { ScriptMeta } from '../_shared/run-main.mts'

const logger = getDefaultLogger()

// Written by the dep-0 installer on every successful apply: the exact file
// list the last bundle owned, one repo-relative path per line.
const APPLIED_FILES_REL = '.cache/fleet/socket-wheelhouse/applied-files'

/**
 * Repo-relative path in `/` form. `normalizePath` collapses `.`/`..` and
 * duplicate slashes but leaves a backslash alone, and both sides of the
 * membership test have to agree, so separators are folded here.
 */
function toPosix(relPath: string): string {
  return normalizePath(relPath.replaceAll('\\', '/'))
}

export interface UntrackAudit {
  readonly mustStayTracked: string[]
  readonly unrecoverable: string[]
}

// A gitignore line that is not a literal repo-relative file path: empty, a
// comment, a negation, an anchored entry, a glob, or a directory entry whose
// trailing slash makes it a subtree rather than a file.
const NON_PATH_ENTRY_RE = /^$|^[#!/]|[*]|\/$/

/**
 * The per-file ignore entries inside a `.gitignore` fleet block. Only literal
 * paths are auditable against a manifest, so every pattern form is skipped.
 * Returns an empty array when the file carries no fleet region, a fat member.
 */
export function fleetBlockPathEntries(gitignoreText: string): string[] {
  const lines = gitignoreText.split('\n')
  const regions = findFleetRegions(lines)
  const out: string[] = []
  for (let i = 0, { length } = regions; i < length; i += 1) {
    const region = regions[i]!
    for (let j = region.start + 1; j < region.end; j += 1) {
      const entry = lines[j]!.trim()
      // Only LITERAL, repo-relative file paths are auditable against a
      // manifest. Everything else in the block is a pattern the thin
      // conversion never generated per-file. A comment, a blank, a negation,
      // a glob, an anchored entry such as `/.cursor`, and a directory entry
      // such as `build/` all fail to name a single bundle file, so looking
      // one up would report a false positive. The test runs on the RAW line:
      // normalizing first would strip the trailing slash that identifies a
      // directory entry.
      if (NON_PATH_ENTRY_RE.test(entry)) {
        continue
      }
      out.push(toPosix(entry))
    }
  }
  return out
}

/**
 * Audit the untrack set against what the bundle actually ships. Pure; the
 * whole point of the check and the part unit tests drive directly.
 */
export function auditUntrackSet(
  ignored: readonly string[],
  bundleFiles: readonly string[],
): UntrackAudit {
  const shipped = new Set(bundleFiles.map(toPosix))
  const mustStayTracked: string[] = []
  const unrecoverable: string[] = []
  for (let i = 0, { length } = ignored; i < length; i += 1) {
    const entry = toPosix(ignored[i]!)
    if (isAlwaysTrackedSurface(entry)) {
      mustStayTracked.push(entry)
      continue
    }
    // A bare name carrying no separator, `.DS_Store` or `node_modules`, is a
    // gitignore PATTERN that matches at any depth rather than a per-file
    // entry the conversion generated, so the bundle is not expected to ship
    // it. The always-tracked test above still sees those entries, which is
    // what matters for a root dotfile such as `.npmrc`.
    if (normalizePath(entry).includes('/') && !shipped.has(entry)) {
      unrecoverable.push(entry)
    }
  }
  return { mustStayTracked, unrecoverable }
}

export async function main(): Promise<void> {
  const quiet = process.argv.includes('--quiet')
  const gitignorePath = path.join(REPO_ROOT, '.gitignore')
  const appliedPath = path.join(REPO_ROOT, APPLIED_FILES_REL)
  if (!existsSync(gitignorePath) || !existsSync(appliedPath)) {
    if (!quiet) {
      logger.log(
        'thin-untracks-are-recoverable: no fleet gitignore block or no applied-files marker — vacuous pass.',
      )
    }
    process.exitCode = 0
    return
  }

  const ignored = fleetBlockPathEntries(readFileSync(gitignorePath, 'utf8'))
  if (ignored.length === 0) {
    if (!quiet) {
      logger.log(
        'thin-untracks-are-recoverable: no per-file fleet ignore entries — not a thin member, vacuous pass.',
      )
    }
    process.exitCode = 0
    return
  }

  const bundleFiles = readFileSync(appliedPath, 'utf8')
    .split('\n')
    .filter(Boolean)
  const { mustStayTracked, unrecoverable } = auditUntrackSet(
    ignored,
    bundleFiles,
  )

  if (mustStayTracked.length === 0 && unrecoverable.length === 0) {
    if (!quiet) {
      logger.log(
        `thin-untracks-are-recoverable: all ${ignored.length} untracked path(s) are shipped by the bundle.`,
      )
    }
    process.exitCode = 0
    return
  }

  if (unrecoverable.length > 0) {
    logger.fail(
      `thin-untracks-are-recoverable: ${unrecoverable.length} untracked path(s) the bundle does NOT ship:`,
    )
    for (let i = 0, { length } = unrecoverable; i < length; i += 1) {
      logger.fail(`  ${unrecoverable[i]!}`)
    }
    logger.fail(
      '  What:   these paths are gitignored and untracked, but no bundle file\n' +
        '          restores them — a fresh clone simply loses them.\n' +
        `  Where:  the .gitignore fleet block, vs ${APPLIED_FILES_REL}.\n` +
        '  Wanted: every untracked path present in the applied bundle.\n' +
        '  Fix:    if the path is repo-owned, move it under scripts/repo/ (or\n' +
        '          its repo-scoped home) and re-track it; if it is stale fleet\n' +
        '          content superseded upstream, delete it.',
    )
  }

  if (mustStayTracked.length > 0) {
    logger.fail(
      `thin-untracks-are-recoverable: ${mustStayTracked.length} always-tracked path(s) are untracked:`,
    )
    for (let i = 0, { length } = mustStayTracked; i < length; i += 1) {
      logger.fail(`  ${mustStayTracked[i]!}`)
    }
    logger.fail(
      '  What:   a surface a consumer reads BEFORE our fetch runs is ignored.\n' +
        '          pnpm reads .npmrc and patches at install time; GitHub reads\n' +
        '          workflows and dependabot.yml from the committed tree.\n' +
        '  Where:  the .gitignore fleet block.\n' +
        '  Wanted: no isAlwaysTrackedSurface path in the ignore block.\n' +
        '  Fix:    drop the entry from the block and `git add` the path back.',
    )
  }
  process.exitCode = 1
}

/* c8 ignore start - entrypoint guard; exercised via subprocess */
const SCRIPT_META: ScriptMeta = {
  describe:
    'checks every path a thin member untracks is shipped by the bundle, so nothing is lost',
  help: `Usage: node scripts/fleet/check/thin-untracks-are-recoverable.mts [flags]
  --quiet  suppress the success message`,
}

if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
/* c8 ignore stop */
