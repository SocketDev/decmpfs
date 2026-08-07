#!/usr/bin/env node
/*
 * @file `check --all` gate: every GitHub artifact reference names immutable
 *   content and carries a human label. A bare release tag, branch name, or
 *   `latest` is a mutable pointer - re-tagging moves it, so the same
 *   reference serves different bytes tomorrow. Three shapes are scanned:
 *
 *   1. `releases/download/<ref>/...` inside a URL.
 *   2. `gh release download <ref>`.
 *   3. `github.com/<owner>/<repo>/archive|raw/<ref>`.
 *
 *   A ref complies as a FULL 40-hex commit sha with a trailing ` # <label>`
 *   comment on the same line, so the human-readable version rides beside the
 *   pin, or as a CONTENT-ADDRESSED name whose final segment is a hex content
 *   hash of 7 or more characters, which needs no label because the name is
 *   already content-unique by construction.
 *
 *   Parsing is deliberately string-based rather than one regex. A pattern
 *   matching a URL path needs nested quantifiers, a catastrophic backtracking
 *   hazard on adversarial input, and a scanner reading untrusted workflow
 *   text is exactly where that must not exist.
 *
 *   Scope: TRACKED workflow, Dockerfile, and external-tools sources.
 *   Generated `*.lock.yml` artifacts are skipped: a compiled lock is
 *   rewritten wholesale by its compiler and is not ours to pin.
 *
 *   MODE: REPORT-ONLY, mirroring `published-packages-have-files-field.mts`.
 *   Flip `MODE` to 'strict' as the follow-up once the backlog is cleared -
 *   a hard gate on a pre-existing backlog ships red fleet-wide.
 *
 *   Exit: 0 - every reference complies, none exist, or MODE is 'report' even
 *   with findings; 1 - a finding exists AND MODE is 'strict'.
 *
 *   Usage: node scripts/fleet/check/external-refs-carry-sha-and-label.mts [--quiet]
 */

import { readFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'

import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

import { REPO_ROOT } from '../paths.mts'
import { isMainModule } from '../_shared/is-main-module.mts'
import { runMain } from '../_shared/run-main.mts'

import type { ScriptMeta } from '../_shared/run-main.mts'

const logger = getDefaultLogger()

// Flip to 'strict' after the pre-existing backlog is clear. In strict mode a
// finding is a hard failure, exit 1. In report mode every finding is logged
// and the gate exits 0 so it never breaks CI on a pre-existing backlog.
const MODE: 'report' | 'strict' = 'report'

const FULL_SHA_LENGTH = 40
const MIN_CONTENT_HASH_LENGTH = 7

// Suffixes a codeload archive URL appends to the ref segment.
const ARCHIVE_SUFFIXES = ['.tar.gz', '.tgz', '.zip']

const GH_DOWNLOAD_MARKER = 'gh release download '
const GITHUB_HOST_MARKER = 'github.com/'
const LABEL_MARKER = ' # '
const RELEASES_DOWNLOAD_MARKER = 'releases/download/'

// Characters that terminate a ref token wherever it appears in a line.
const REF_STOP_CHARS = '\t "#\'),;>`'

export interface GithubRef {
  end: number
  ref: string
}

export interface RefFinding {
  file: string
  kind: 'mutable' | 'unlabeled'
  line: number
  ref: string
}

/**
 * True when every character is a lowercase hex digit. Linear, and used instead
 * of a quantified character class so the scanner has no backtracking surface.
 */
function isHex(text: string): boolean {
  for (let i = 0, { length } = text; i < length; i += 1) {
    const c = text.charCodeAt(i)
    const isDigit = c >= 48 && c <= 57
    const isAf = c >= 97 && c <= 102
    if (!isDigit && !isAf) {
      return false
    }
  }
  return text.length > 0
}

/*
 * Breakdown of the only regex in this file: `\r?` is an optional carriage
 * return, `\n` is the newline, so CRLF and LF sources split identically.
 */
function splitLines(text: string): string[] {
  return text.split(/\r?\n/)
}

/**
 * True when a ref is a full 40-hex commit sha. A full sha names one commit
 * forever, so it is the pinned form; the label requirement rides on top.
 */
export function isFullCommitSha(ref: string): boolean {
  return ref.length === FULL_SHA_LENGTH && isHex(ref)
}

/**
 * True when a ref names immutable content by construction: its final `-`,
 * `_`, or `.` separated segment is a hex hash of
 * {@link MIN_CONTENT_HASH_LENGTH} or more characters. `bundle-7db5bc4592dda`
 * passes; `v1.2.3` and `latest` do not. Pure; exported for tests.
 */
export function isContentAddressedName(ref: string): boolean {
  let start = -1
  for (let i = ref.length - 1; i >= 0; i -= 1) {
    const c = ref[i]!
    if (c === '_' || c === '-' || c === '.') {
      start = i + 1
      break
    }
  }
  const segment = start === -1 ? ref : ref.slice(start)
  return segment.length >= MIN_CONTENT_HASH_LENGTH && isHex(segment)
}

/**
 * True when the text after a ref carries a ` # <label>` comment with a
 * non-empty label. The label is the human-readable version the sha replaced.
 */
export function hasTrailingLabel(textAfterRef: string): boolean {
  const at = textAfterRef.indexOf(LABEL_MARKER)
  if (at === -1) {
    return false
  }
  return textAfterRef.slice(at + LABEL_MARKER.length).trim().length > 0
}

/**
 * Read a whole ref token from `start` until a stop character. Linear scan,
 * no regex. Used for the `gh release download <ref>` argument, where a `/`
 * may be part of the ref itself.
 */
function readToken(line: string, start: number): { end: number; text: string } {
  let end = start
  while (end < line.length && !REF_STOP_CHARS.includes(line[end]!)) {
    end += 1
  }
  return { end, text: line.slice(start, end) }
}

/**
 * Read one URL path segment from `start`, stopping at `/` as well as the
 * usual stop characters. Linear scan, no regex.
 */
function readPathSegment(
  line: string,
  start: number,
): { end: number; text: string } {
  let end = start
  while (end < line.length) {
    const c = line[end]!
    if (c === '/' || REF_STOP_CHARS.includes(c)) {
      break
    }
    end += 1
  }
  return { end, text: line.slice(start, end) }
}

/**
 * Strip the archive suffix a codeload URL appends, so `abc123….tar.gz`
 * classifies by its ref text alone.
 */
function stripArchiveSuffix(ref: string): string {
  for (let i = 0, { length } = ARCHIVE_SUFFIXES; i < length; i += 1) {
    const suffix = ARCHIVE_SUFFIXES[i]!
    if (ref.endsWith(suffix)) {
      return ref.slice(0, -suffix.length)
    }
  }
  return ref
}

/**
 * The ref inside a `github.com/<owner>/<repo>/archive|raw/<ref>` URL whose
 * host marker ends at `start`, or undefined when the path is another GitHub
 * surface. An `archive/refs/tags/<tag>` or `archive/refs/heads/<branch>`
 * path yields the tag or branch segment, the one a human chose.
 */
function parseGithubUrlRef(line: string, start: number): GithubRef | undefined {
  const owner = readPathSegment(line, start)
  if (!owner.text || line[owner.end] !== '/') {
    return undefined
  }
  const repo = readPathSegment(line, owner.end + 1)
  if (!repo.text || line[repo.end] !== '/') {
    return undefined
  }
  const kind = readPathSegment(line, repo.end + 1)
  if (kind.text !== 'archive' && kind.text !== 'raw') {
    return undefined
  }
  if (line[kind.end] !== '/') {
    return undefined
  }
  let seg = readPathSegment(line, kind.end + 1)
  if (seg.text === 'refs' && line[seg.end] === '/') {
    const middle = readPathSegment(line, seg.end + 1)
    if (!middle.text || line[middle.end] !== '/') {
      return undefined
    }
    seg = readPathSegment(line, middle.end + 1)
  }
  if (!seg.text) {
    return undefined
  }
  return { end: seg.end, ref: stripArchiveSuffix(seg.text) }
}

/**
 * Every GitHub artifact ref in one line, with the index just past each ref so
 * the caller can look for a trailing label. Pure; exported for tests.
 */
export function findGithubRefsInLine(line: string): GithubRef[] {
  const out: GithubRef[] = []
  let from = line.indexOf(RELEASES_DOWNLOAD_MARKER)
  while (from !== -1) {
    const start = from + RELEASES_DOWNLOAD_MARKER.length
    const seg = readPathSegment(line, start)
    if (seg.text) {
      out.push({ end: seg.end, ref: seg.text })
    }
    from = line.indexOf(RELEASES_DOWNLOAD_MARKER, start)
  }
  from = line.indexOf(GH_DOWNLOAD_MARKER)
  while (from !== -1) {
    let start = from + GH_DOWNLOAD_MARKER.length
    while (start < line.length && line[start] === ' ') {
      start += 1
    }
    const token = readToken(line, start)
    if (token.text && !token.text.startsWith('-')) {
      out.push({ end: token.end, ref: token.text })
    }
    from = line.indexOf(GH_DOWNLOAD_MARKER, start)
  }
  from = line.indexOf(GITHUB_HOST_MARKER)
  while (from !== -1) {
    const start = from + GITHUB_HOST_MARKER.length
    const parsed = parseGithubUrlRef(line, start)
    if (parsed) {
      out.push(parsed)
    }
    from = line.indexOf(GITHUB_HOST_MARKER, start)
  }
  return out
}

/**
 * Every violating GitHub artifact reference in `text`: a mutable ref, or a
 * full-sha ref missing its ` # <label>` comment. Pure; exported for tests.
 */
export function findRefFindings(text: string, file: string): RefFinding[] {
  const out: RefFinding[] = []
  const lines = splitLines(text)
  for (let i = 0, { length } = lines; i < length; i += 1) {
    const line = lines[i]!
    const refs = findGithubRefsInLine(line)
    for (let j = 0, { length: refCount } = refs; j < refCount; j += 1) {
      const { end, ref } = refs[j]!
      if (isFullCommitSha(ref)) {
        if (!hasTrailingLabel(line.slice(end))) {
          out.push({ file, kind: 'unlabeled', line: i + 1, ref })
        }
        continue
      }
      if (isContentAddressedName(ref)) {
        continue
      }
      out.push({ file, kind: 'mutable', line: i + 1, ref })
    }
  }
  return out
}

/**
 * Total GitHub artifact refs in `text`, compliant ones included, so the
 * report always states its denominator.
 */
export function countGithubRefs(text: string): number {
  let count = 0
  const lines = splitLines(text)
  for (let i = 0, { length } = lines; i < length; i += 1) {
    count += findGithubRefsInLine(lines[i]!).length
  }
  return count
}

/**
 * True when a path is a GENERATED artifact this check must not gate. A gh-aw
 * `*.lock.yml` is the motivating case: its references belong to the bundle
 * its compiler emits, and a hand pin would be overwritten on the next
 * compile.
 */
export function isGeneratedSource(relPath: string): boolean {
  return relPath.endsWith('.lock.yml')
}

/**
 * Tracked workflow, Dockerfile, and external-tools sources. Tracked-only
 * keeps build output and vendored trees out of the scan.
 */
export function listExternalRefSources(repoRoot: string): string[] {
  const result = spawnSync(
    'git',
    [
      'ls-files',
      '*.yml',
      '*.yaml',
      'Dockerfile*',
      '*.Dockerfile',
      'scripts/fleet/external-tools/*.mts',
    ],
    { cwd: repoRoot, stdio: 'pipe', stdioString: true },
  )
  if (result.status !== 0) {
    return []
  }
  return splitLines(String(result.stdout ?? ''))
    .map(s => s.trim())
    .filter(rel => rel.length > 0 && !isGeneratedSource(rel))
}

export function main(): number {
  const quiet = process.argv.includes('--quiet')
  const sources = listExternalRefSources(REPO_ROOT)
  const findings: RefFinding[] = []
  let refTotal = 0
  for (let i = 0, { length } = sources; i < length; i += 1) {
    const rel = sources[i]!
    let text: string
    try {
      text = readFileSync(path.join(REPO_ROOT, rel), 'utf8')
    } catch {
      continue
    }
    refTotal += countGithubRefs(text)
    findings.push(...findRefFindings(text, rel))
  }

  const inventory = `${refTotal} reference(s) across ${sources.length} source(s)`
  if (findings.length === 0) {
    if (!quiet) {
      logger.success(
        `[external-refs-carry-sha-and-label] ${inventory}: every GitHub artifact reference is sha-pinned and labeled.`,
      )
    }
    return 0
  }

  const isStrict = MODE === 'strict'
  logger.fail(
    `[external-refs-carry-sha-and-label]${isStrict ? '' : ' (report-only)'} GitHub artifact references violating the sha-and-label contract:`,
  )
  logger.error(
    `  What:   ${findings.length} finding(s) in ${inventory}. A mutable ref can serve different bytes tomorrow while the reference stays byte-identical; a sha with no label hides which release a human is reading.`,
  )
  logger.error('  Where:  each reference listed below.')
  for (let i = 0, { length } = findings; i < length; i += 1) {
    const f = findings[i]!
    logger.substep(`${f.file}:${f.line}  [${f.kind}]  ${f.ref}`)
  }
  logger.error(
    '  Saw:    a mutable tag or an unlabeled sha; wanted `<40-hex sha> # <label>`, or a content-addressed name whose final segment is the content hash.',
  )
  logger.error(
    '  Fix:    resolve the sha once with `gh api repos/<o>/<r>/git/ref/tags/<tag>`, reference the sha, and append ` # <tag>`. When no tag exists, pin the branch head sha and label it `<branch> <YYYY-MM-DD>`.',
  )
  if (!isStrict) {
    logger.log(
      `[external-refs-carry-sha-and-label] report-only mode: exiting 0 with ${findings.length} finding(s). Flip MODE to 'strict' once the backlog above is cleared.`,
    )
    return 0
  }
  return 1
}

const SCRIPT_META: ScriptMeta = {
  describe:
    'every github artifact reference is sha-pinned and carries a human label',
  help: `Usage: node scripts/fleet/check/external-refs-carry-sha-and-label.mts [--quiet]`,
}

/* c8 ignore start - entry-point wiring, exercised by running the script. */
if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
/* c8 ignore stop */
