#!/usr/bin/env node
/*
 * @file Run the pre-push gate's CHEAP checks locally, in one pass, so the whole
 *   cheap blocker set is known before a push instead of after it.
 *
 *   The waste this exists to stop, measured in a real session: a push was used
 *   as the error-discovery loop. Push, read one stage's errors, fix, push again.
 *   Each cycle paid a full gate run (lint over 2,591 files, a whole-project
 *   tsc, a cascade-drift scan) plus a network round trip, and it took five
 *   attempts to land what one local pass would have listed up front.
 *
 *   Two behaviours, and the split is the whole design:
 *
 *   - The cheap checks ACCUMULATE. Lint, type, and drift are independent — a
 *     lint failure says nothing about whether types pass — so all of them run
 *     and every red is reported together.
 *   - The suite FAILS FAST. It runs only under `--tests`, and only when the
 *     cheap set is clean. A suite verdict gathered beside a lint red is thrown
 *     away the moment that red is fixed and everything re-runs, and every run
 *     being a full run is not workable.
 *
 *   Cheap by default for the same reason: a preflight that runs the whole suite
 *   every time is one nobody runs.
 *
 *   The git pre-push hook is a separate surface and already accumulates
 *   (`totalErrors +=` in `.git-hooks/fleet/pre-push.mts`), so a blocked push has
 *   its whole set on screen too. `scripts/fleet/pre-push-gate.mts`, the skill's
 *   front door, is the one that used to stop at the first red.
 *
 *   THE TEMPLATE TRAP, and the reason this is a script rather than a note: in
 *   the wheelhouse, `pnpm run type` and `pnpm test` read the LIVE tree, while a
 *   fleet-canonical edit lands in `template/base/`. Until the cascade copies it
 *   across, every result about that edit describes the OLD code — a green that
 *   means nothing and a red that sends you fixing a file you already fixed.
 *   The live mirrors are `chmod 0o444`, so the cascade is the only way to move
 *   them, and the cascade refuses while `template/` is dirty. That ordering is
 *   not discoverable from an error message, so this script enforces it: dirty
 *   template sources are reported FIRST, and the verdict says the run is stale
 *   rather than pretending the numbers mean something.
 *
 *   Exit: 0 every stage it ran is clean and the tree is not stale; 1 any stage
 *   failed, or the run was stale.
 *
 *   Usage: node scripts/fleet/preflight.mts [--tests]
 */

import { existsSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'

import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

import { isMainModule } from './_shared/is-main-module.mts'
import { runMain } from './_shared/run-main.mts'
import { REPO_ROOT } from './paths.mts'

import type { ScriptMeta } from './_shared/run-main.mts'

const logger = getDefaultLogger()

/**
 * One check to run, in the order the pre-push gate runs them.
 */
export interface Stage {
  /**
   * Argv after the node binary.
   */
  argv: readonly string[]
  /**
   * What to do when it fails, in the operator's words.
   */
  fix: string
  /**
   * Display name.
   */
  name: string
  /**
   * Skip silently when this path is absent — a non-wheelhouse member has no
   * `template/base`, and a repo without a tsconfig has nothing to type-check.
   */
  requires?: string | undefined
}

const TYPE_CHECK_TSCONFIG = path.join('.config', 'fleet', 'tsconfig.check.json')

/**
 * The gate's stages, as a table. Kept in the gate's own order so a reader can
 * line the two up; adding a gate stage means adding a row here.
 */
export const STAGES: readonly Stage[] = [
  {
    argv: [path.join('scripts', 'fleet', 'lint.mts'), '--all'],
    fix: 'pnpm run fix --all',
    name: 'lint + format',
  },
  {
    argv: [
      path.join('node_modules', 'typescript', 'bin', 'tsc'),
      '--noEmit',
      '-p',
      TYPE_CHECK_TSCONFIG,
    ],
    fix: 'resolve the type error(s), then re-run',
    name: 'type',
    requires: TYPE_CHECK_TSCONFIG,
  },
  {
    argv: [
      path.join('scripts', 'fleet', 'check', 'dispatch-table-is-current.mts'),
      '--quiet',
    ],
    fix: 'node scripts/fleet/build-hook-bundle.mts, then commit the regen',
    name: 'hook dispatch table',
    requires: 'template/base',
  },
]

const TEST_STAGE: Stage = {
  argv: [path.join('scripts', 'fleet', 'test.mts')],
  fix: 'fix the failing test(s), then re-run',
  name: 'test',
}

/**
 * Template sources with uncommitted changes, repo-relative.
 *
 * These make every later stage stale: the checks read the live tree, so a
 * template edit that has not cascaded is invisible to them. Empty on any
 * porcelain failure — a preflight must never be the thing that breaks.
 */
export function dirtyTemplateSources(repoRoot: string): string[] {
  const result = spawnSync(
    'git',
    ['status', '--porcelain', '--', 'template/'],
    { cwd: repoRoot, stdio: 'pipe', stdioString: true },
  )
  if (result.status !== 0 || typeof result.stdout !== 'string') {
    return []
  }
  const out: string[] = []
  const lines = result.stdout.replace(/\r\n/g, '\n').split('\n')
  for (let i = 0, { length } = lines; i < length; i += 1) {
    const line = lines[i]!
    if (line.length > 3) {
      out.push(line.slice(3).trim())
    }
  }
  return out
}

/**
 * Run `stage`, or undefined when its `requires` path is absent.
 */
export function runStage(stage: Stage): boolean | undefined {
  if (stage.requires !== undefined && !existsSync(stage.requires)) {
    return undefined
  }
  logger.info(`[preflight] ${stage.name}…`)
  const r = spawnSync(process.execPath, [...stage.argv], {
    // lint.mts shells out to pnpm, which aborts in a non-TTY without this —
    // the same reason the pre-push gate sets it.
    env: {
      __proto__: null,
      ...process.env,
      CI: 'true',
    } as unknown as NodeJS.ProcessEnv,
    stdio: 'inherit',
  })
  return r.status === 0
}

export function main(argv: readonly string[] = process.argv.slice(2)): number {
  // Tests are OPT-IN. A preflight that runs the whole suite every time is one
  // nobody runs, which defeats the point — the cheap set is what you want on
  // every iteration, and the suite once before the push.
  const withTests = argv.includes('--tests') || argv.includes('--full')
  const stale = dirtyTemplateSources(REPO_ROOT)
  if (stale.length) {
    logger.warn(
      [
        `[preflight] ${stale.length} template source(s) are uncommitted, so every result below is STALE:`,
        ...stale.slice(0, 10).map(p => `    ${p}`),
        stale.length > 10 ? `    …and ${stale.length - 10} more` : '',
        '',
        '  The checks read the LIVE tree. A template edit is invisible to them',
        '  until the cascade copies it across, and the live mirrors are read-only',
        '  so the cascade is the only way to move them. Commit these, run',
        '  `pnpm run dogfood`, then re-run preflight — otherwise a red here may',
        '  name a file you already fixed.',
      ]
        .filter(Boolean)
        .join('\n'),
    )
  }
  const failed: string[] = []
  const skipped: string[] = []
  for (let i = 0, { length } = STAGES; i < length; i += 1) {
    const stage = STAGES[i]!
    const ok = runStage(stage)
    if (ok === undefined) {
      skipped.push(stage.name)
    } else if (!ok) {
      failed.push(stage.name)
    }
  }
  // Fail fast before the suite: a cheap red will be fixed and everything
  // re-runs, so a suite verdict gathered now is thrown away.
  if (withTests && !failed.length) {
    if (!runStage(TEST_STAGE)) {
      failed.push(TEST_STAGE.name)
    }
  } else if (withTests) {
    logger.info(
      '[preflight] tests not run — fix the cheap reds above first, then re-run.',
    )
  }
  logger.error('')
  if (skipped.length) {
    logger.info(`[preflight] not applicable here: ${skipped.join(', ')}`)
  }
  if (!failed.length && !stale.length) {
    logger.success(
      withTests
        ? '[preflight] every stage is clean, suite included — safe to push.'
        : '[preflight] cheap stages clean. Run `pnpm run preflight --tests` once before pushing.',
    )
    return 0
  }
  if (failed.length) {
    logger.fail(`[preflight] ${failed.length} stage(s) failed:`)
    const all = [...STAGES, TEST_STAGE]
    for (let i = 0, { length } = all; i < length; i += 1) {
      const stage = all[i]!
      if (failed.includes(stage.name)) {
        logger.error(`    ${stage.name} — ${stage.fix}`)
      }
    }
    logger.info(
      '  Fix EVERY one above before pushing. They were all reported here on' +
        ' purpose: the push gate reports the same set, and discovering them' +
        ' one push at a time costs a full gate run each time.',
    )
  }
  if (stale.length && !failed.length) {
    logger.fail(
      '[preflight] stages passed, but against a stale tree — cascade, then re-run.',
    )
  }
  return 1
}

const SCRIPT_META: ScriptMeta = {
  describe:
    "runs the pre-push gate's cheap stages locally in one pass, so the whole cheap blocker set is known before pushing",
  help: `Usage: node scripts/fleet/preflight.mts [--tests]

  Runs lint+format, type, and hook-dispatch drift — the cheap half of the
  pre-push gate — and reports EVERY failure together rather than stopping at the
  first. Cheap by default on purpose: a preflight that runs the whole suite every
  time is one nobody runs.

  --tests (or --full) adds the suite, and only when the cheap stages are clean:
  a cheap red gets fixed and everything re-runs, so a suite verdict gathered
  alongside it is wasted time.

  In the wheelhouse it reports uncommitted template/ sources first: the checks
  read the live tree, so a template edit is invisible to them until the cascade
  copies it across, and a result gathered before that is stale.

  --tests        also run the suite (only if the cheap stages pass).`,
}

if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
