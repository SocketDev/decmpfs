#!/usr/bin/env node
/*
 * @file Every `socket/<rule>` a suppression names has to exist. A directive
 *   naming a rule that does not is INERT: it suppresses nothing, the rule it was
 *   meant to waive keeps firing, and the author reads the comment and believes
 *   they hold a waiver. Nothing else catches it — oxlint ignores an unknown rule
 *   in a disable comment rather than complaining, which is the property that
 *   lets the fleet's hook-only scanners claim a `socket/` name, and the same
 *   property that makes a typo silent.
 *
 *   Found one in the wild: `socket/prefer-node-modules-dot-cache` for what is
 *   actually `socket/prefer-repo-root-dot-cache`. It had been sitting there
 *   doing nothing.
 *
 *   Known names come from two places, because a rule name has two homes: a
 *   directory under the oxlint plugin's `fleet/`, or the right-hand side of
 *   `MARKER_RULE_NAMES` for the commit-time and edit-time scanners that own a
 *   name without owning a plugin rule.
 */

import { readdirSync, readFileSync } from 'node:fs'
import path from 'node:path'

import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { normalizePath } from '@socketsecurity/lib-stable/paths/normalize'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

import { splitLines } from '../../../.git-hooks/_shared/scan-core.mts'
import { isNeverGated } from '../_shared/format-scope.mts'
import { isMainModule } from '../_shared/is-main-module.mts'
import { runMain } from '../_shared/run-main.mts'
import type { ScriptMeta } from '../_shared/run-main.mts'
import { REPO_ROOT } from '../paths.mts'

const logger = getDefaultLogger()

/**
 * A suppression directive at a comment opener, capturing its rule list.
 *
 * Anchored so a directive quoted inside a string literal is not read as one — a
 * rule's own test corpus names synthetic rules (`socket/example-rule`) on
 * purpose, and flagging those would make the check unusable.
 */
const DIRECTIVE_RE =
  /^[^'"`\n]*?(?:#|\/\*|\/\/)\s*(?:eslint|oxlint)-disable(?:-line|-next-line)?\s+(?<rules>[\w/,\s-]+?)(?:\s--\s|\s*\*\/|$)/

/**
 * A JSDoc continuation line, where a directive is being SHOWN rather than
 * applied — the rule docs illustrate the shapes they forbid.
 */
const JSDOC_LINE_RE = /^\s*\*/

const PLUGIN_RULES_DIR = '.config/fleet/oxlint-plugin/fleet'
const SUPPRESSION_RULES_PATH =
  '.claude/hooks/fleet/_shared/suppression-rules.mts'

/**
 * Every `socket/<rule>` name the fleet answers to: one per plugin rule
 * directory, plus the scanner-owned names the marker table maps onto.
 *
 * Exported so the name set can be asserted directly.
 */
export function knownRuleNames(repoRoot: string): Set<string> {
  const known = new Set<string>()
  for (const layer of ['template/base', '.']) {
    let entries: string[]
    try {
      entries = readdirSync(path.join(repoRoot, layer, PLUGIN_RULES_DIR))
    } catch {
      continue
    }
    for (let i = 0, { length } = entries; i < length; i += 1) {
      known.add(entries[i]!)
    }
  }
  let table = ''
  try {
    table = readFileSync(
      path.join(repoRoot, 'template/base', SUPPRESSION_RULES_PATH),
      'utf8',
    )
  } catch {
    table = ''
  }
  for (const m of table.matchAll(/'socket\/([\w-]+)'/g)) {
    known.add(m[1]!)
  }
  return known
}

/**
 * The `socket/` rule names `text` suppresses that are not in `known`.
 *
 * Pure and exported: the parse is the part that goes wrong quietly, so it is
 * tested on strings rather than through a tree walk.
 */
export function findInertRuleNames(
  text: string,
  known: ReadonlySet<string>,
): Array<{ line: number; name: string }> {
  const found: Array<{ line: number; name: string }> = []
  const lines = splitLines(text)
  for (let i = 0, { length } = lines; i < length; i += 1) {
    const line = lines[i]!
    if (JSDOC_LINE_RE.test(line)) {
      continue
    }
    const rules = DIRECTIVE_RE.exec(line)?.groups?.['rules']
    if (!rules) {
      continue
    }
    const names = rules.split(',')
    for (let j = 0, count = names.length; j < count; j += 1) {
      const name = names[j]!.trim()
      if (!name.startsWith('socket/')) {
        continue
      }
      if (!known.has(name.slice('socket/'.length))) {
        found.push({ line: i + 1, name })
      }
    }
  }
  return found
}

const CODE_FILE_RE = /\.(?:[cm]?[jt]sx?)$/
// A rule's own test corpus names synthetic rules deliberately.
const TEST_DIR_RE = /^test\//

/**
 * Every tracked code file, repo-relative. Empty on any failure, the fleet
 * convention for these porcelain reads.
 */
function collectCodeFiles(repoRoot: string): string[] {
  const result = spawnSync('git', ['ls-files', '-z'], {
    cwd: repoRoot,
    maxBuffer: 64 * 1024 * 1024,
    stdio: 'pipe',
  })
  if (result.status !== 0) {
    return []
  }
  const { stdout } = result
  const listed = typeof stdout === 'string' ? stdout : String(stdout)
  const entries = listed.split('\0')
  const files: string[] = []
  for (let i = 0, { length } = entries; i < length; i += 1) {
    const raw = entries[i]!
    if (raw) {
      files.push(normalizePath(raw))
    }
  }
  return files
}

export function main(): number {
  const known = knownRuleNames(REPO_ROOT)
  const findings: string[] = []
  const files = collectCodeFiles(REPO_ROOT)
  for (let i = 0, { length } = files; i < length; i += 1) {
    const rel = files[i]!
    if (!CODE_FILE_RE.test(rel) || TEST_DIR_RE.test(rel) || isNeverGated(rel)) {
      continue
    }
    let text: string
    try {
      text = readFileSync(path.join(REPO_ROOT, rel), 'utf8')
    } catch {
      continue
    }
    for (const hit of findInertRuleNames(text, known)) {
      findings.push(`  ${rel}:${hit.line} — \`${hit.name}\` is not a rule`)
    }
  }
  if (!findings.length) {
    logger.success(
      `[suppressed-rules-resolve] every suppression names one of ${known.size} known rules.`,
    )
    return 0
  }
  logger.error(
    [
      '[suppressed-rules-resolve] suppression(s) naming a rule that does not exist:',
      ...findings,
      '',
      '  A directive naming an unknown rule is INERT — oxlint ignores it rather',
      '  than complaining, so the rule it was meant to waive keeps firing while',
      '  the comment says otherwise. Fix the name, or add the rule.',
    ].join('\n'),
  )
  return 1
}

const SCRIPT_META: ScriptMeta = {
  describe:
    'checks that every suppression names a rule that exists — a directive naming an unknown rule is inert',
  help: `Usage: node scripts/fleet/check/suppressed-rules-resolve.mts

  Reads every tracked code file and reports a \`socket/<rule>\` in a disable
  directive that matches neither an oxlint plugin rule directory nor a
  scanner-owned name in the marker table. oxlint ignores an unknown rule in a
  disable comment rather than complaining, so a typo there is otherwise silent.`,
}

if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
