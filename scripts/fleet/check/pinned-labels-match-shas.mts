#!/usr/bin/env node
/*
 * @file Release/CI-tier gate: every `<40-hex sha> # v<label>` pin still names
 *   the sha its upstream tag resolves to. The sha is what the machine trusts;
 *   the label is prose for humans. When an upstream tag MOVES the two
 *   silently diverge and a moved tag is a supply-chain signal to read first.
 *
 *   NETWORK DISCIPLINE. Resolution shells `gh api`, so this check belongs to
 *   the release/CI tier and the interactive `check --all` loop stays offline.
 *   No `gh`, no network, or an unconfident answer yields a LOUD skip and
 *   exit 0; only a label resolving to a DIFFERENT sha, or no tag, fails.
 *
 *   Parsing is linear substring work with no nested-quantifier regex, since
 *   the scanner reads untrusted workflow text. A sha-pin whose line names no
 *   resolvable `<owner>/<repo>` is counted and printed as `unresolvable`,
 *   never silently dropped.
 *
 *   Exit: 0 - labels match, nothing to verify, or offline skip; 1 - a pinned
 *   label moved or its tag is gone.
 *   Usage: node scripts/fleet/check/pinned-labels-match-shas.mts [--quiet]
 */

import { readFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'

import { errorMessage } from '@socketsecurity/lib-stable/errors/message'
import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import {
  spawn,
  spawnSync,
} from '@socketsecurity/lib-stable/process/spawn/child'

import { isMainModule } from '../_shared/is-main-module.mts'
import { runMain } from '../_shared/run-main.mts'
import { REPO_ROOT } from '../paths.mts'

import type { ScriptMeta } from '../_shared/run-main.mts'

const logger = getDefaultLogger()

const GITHUB_HOST = 'github.com/'
const LABEL_MARKER = ' # v'
const SHA_LENGTH = 40
const USES_KEY = 'uses:'

/**
 * A `<sha> # v<label>` pin whose line also names the `<owner>/<repo>` the
 * label can be resolved against.
 */
export interface TagPin {
  file: string
  label: string
  line: number
  owner: string
  repo: string
  sha: string
}

/**
 * A `<sha> # v<label>` pin whose line names NO resolvable repo. Reported and
 * counted so the operator sees what the gate could not verify.
 */
export interface UnresolvablePin {
  file: string
  label: string
  line: number
  sha: string
}

/**
 * True when the char code is a lowercase hex digit. Charcode arithmetic keeps
 * the scanner linear with no quantified character class.
 */
function isHexCode(code: number): boolean {
  const isAf = code >= 97 && code <= 102
  const isDigit = code >= 48 && code <= 57
  return isAf || isDigit
}

/**
 * True when `text` is exactly {@link SHA_LENGTH} lowercase hex characters.
 */
export function isFullSha(text: string): boolean {
  if (text.length !== SHA_LENGTH) {
    return false
  }
  for (let i = 0; i < SHA_LENGTH; i += 1) {
    if (!isHexCode(text.charCodeAt(i))) {
      return false
    }
  }
  return true
}

/**
 * The `<sha> # v<label>` pair on one line, or undefined when the line does
 * not carry one. The sha is the 40 hex characters immediately before the
 * ` # v` marker; the label is `v` plus a digits-and-dots run that must end at
 * whitespace or the end of the line, so a suffixed label like `v1.2.3-rc1` is
 * left alone rather than half-parsed.
 */
export function parseShaAndLabel(
  line: string,
): { label: string; sha: string } | undefined {
  const markerIdx = line.indexOf(LABEL_MARKER)
  if (markerIdx < SHA_LENGTH) {
    return undefined
  }
  const sha = line.slice(markerIdx - SHA_LENGTH, markerIdx)
  if (!isFullSha(sha)) {
    return undefined
  }
  const beforeIdx = markerIdx - SHA_LENGTH - 1
  if (beforeIdx >= 0 && isHexCode(line.charCodeAt(beforeIdx))) {
    // A hex run longer than 40 characters is not a sha-pin.
    return undefined
  }
  let end = markerIdx + LABEL_MARKER.length
  let sawDigit = false
  while (end < line.length) {
    const code = line.charCodeAt(end)
    if (code >= 48 && code <= 57) {
      sawDigit = true
      end += 1
      continue
    }
    if (line[end] === '.') {
      end += 1
      continue
    }
    break
  }
  if (!sawDigit) {
    return undefined
  }
  if (end < line.length && line[end] !== '\t' && line[end] !== ' ') {
    return undefined
  }
  return { label: line.slice(markerIdx + LABEL_MARKER.length - 1, end), sha }
}

/**
 * Split `owner/repo[/subpath]` into its first two segments. A local action
 * path starting with `.` and a `docker://` reference are rejected, since
 * neither names a GitHub repo a tag can be read from.
 */
function splitOwnerRepo(
  refPath: string,
): { owner: string; repo: string } | undefined {
  if (refPath === '' || refPath[0] === '.' || refPath.includes(':')) {
    return undefined
  }
  const firstSlash = refPath.indexOf('/')
  if (firstSlash <= 0) {
    return undefined
  }
  const owner = refPath.slice(0, firstSlash)
  let rest = refPath.slice(firstSlash + 1)
  const nextSlash = rest.indexOf('/')
  if (nextSlash !== -1) {
    rest = rest.slice(0, nextSlash)
  }
  const repo = rest.endsWith('.git') ? rest.slice(0, -4) : rest
  return repo === '' ? undefined : { owner, repo }
}

/**
 * The repo named by a `uses: owner/repo@...` token on `line`, if any.
 */
function parseUsesRepo(
  line: string,
): { owner: string; repo: string } | undefined {
  const usesIdx = line.indexOf(USES_KEY)
  if (usesIdx === -1) {
    return undefined
  }
  let i = usesIdx + USES_KEY.length
  while (i < line.length && (line[i] === '\t' || line[i] === ' ')) {
    i += 1
  }
  while (i < line.length && (line[i] === "'" || line[i] === '"')) {
    i += 1
  }
  const start = i
  while (
    i < line.length &&
    line[i] !== '\t' &&
    line[i] !== ' ' &&
    line[i] !== '@'
  ) {
    i += 1
  }
  return splitOwnerRepo(line.slice(start, i))
}

/**
 * True for a character that ends a repo path inside a URL or shell text.
 */
function isPathDelimiter(c: string): boolean {
  return (
    c === '\t' ||
    c === ' ' ||
    c === '?' ||
    c === "'" ||
    c === '"' ||
    c === ')' ||
    c === '#' ||
    c === '>'
  )
}

/**
 * The repo named by a `github.com/owner/repo` URL on `line`, if any.
 */
function parseGithubUrlRepo(
  line: string,
): { owner: string; repo: string } | undefined {
  const hostIdx = line.indexOf(GITHUB_HOST)
  if (hostIdx === -1) {
    return undefined
  }
  const start = hostIdx + GITHUB_HOST.length
  let end = start
  while (end < line.length && !isPathDelimiter(line[end]!)) {
    end += 1
  }
  let refPath = line.slice(start, end)
  const at = refPath.indexOf('@')
  if (at !== -1) {
    refPath = refPath.slice(0, at)
  }
  return splitOwnerRepo(refPath)
}

/**
 * The `<owner>/<repo>` named on `line`, read from a `uses: owner/repo@` token
 * or a `github.com/owner/repo` URL, or undefined when neither form is there.
 */
export function parseRepoOnLine(
  line: string,
): { owner: string; repo: string } | undefined {
  return parseUsesRepo(line) ?? parseGithubUrlRepo(line)
}

export interface PinScan {
  pins: TagPin[]
  unresolvable: UnresolvablePin[]
}

/**
 * Scan `text` line by line for `<sha> # v<label>` pins. A pin whose line also
 * names `<owner>/<repo>` lands in `pins`; one that does not lands in
 * `unresolvable` so the report counts it instead of silently dropping it.
 * Lines are 1-indexed. Pure; exported for tests.
 */
export function scanShaPins(text: string, file: string): PinScan {
  const pins: TagPin[] = []
  const unresolvable: UnresolvablePin[] = []
  const lines = text.split(/\r?\n/)
  for (let i = 0, { length } = lines; i < length; i += 1) {
    const line = lines[i]!
    const parsed = parseShaAndLabel(line)
    if (!parsed) {
      continue
    }
    const slug = parseRepoOnLine(line)
    if (slug) {
      pins.push({
        file,
        label: parsed.label,
        line: i + 1,
        owner: slug.owner,
        repo: slug.repo,
        sha: parsed.sha,
      })
    } else {
      unresolvable.push({
        file,
        label: parsed.label,
        line: i + 1,
        sha: parsed.sha,
      })
    }
  }
  return { pins, unresolvable }
}

/**
 * Every resolvable `<sha> # v<label>` pin in `text`. Pure; exported for tests.
 */
export function collectTagPins(text: string, file: string): TagPin[] {
  return scanShaPins(text, file).pins
}

/**
 * The narrow shape this check reads off a `gh api` git-ref payload. The
 * `object.type` distinguishes a lightweight tag, whose sha IS the commit,
 * from an annotated tag, whose sha names a tag object that is dereferenced
 * once more.
 */
export interface GitRefPayload {
  object?:
    | {
        sha?: string | undefined
        type?: string | undefined
      }
    | undefined
}

/**
 * One ref lookup's transport verdict. `gone` means the API answered and the
 * ref does not exist; `unavailable` means no confident answer arrived, which
 * downgrades the pin to a loud skip rather than a failure.
 */
export interface RefLookup {
  payload?: GitRefPayload | undefined
  status: 'gone' | 'ok' | 'unavailable'
}

/**
 * Fetches one `gh api` path. Injected so tests never touch the network.
 */
export type RefResolver = (apiPath: string) => Promise<RefLookup>

/**
 * Text worth matching on a failed spawn: the error message plus any stderr
 * the child left behind, which is where `gh` prints its HTTP status.
 */
function spawnFailureText(e: unknown): string {
  const parts = [errorMessage(e)]
  if (e !== null && typeof e === 'object' && 'stderr' in e) {
    const { stderr } = e as { stderr?: unknown | undefined }
    if (typeof stderr === 'string') {
      parts.push(stderr)
    }
  }
  return parts.join(' ')
}

/**
 * The default resolver: shells `gh api <path>`. An HTTP 404 reads as `gone`;
 * a missing binary, dead network, or any other refusal reads as
 * `unavailable`.
 */
export async function ghRefResolver(apiPath: string): Promise<RefLookup> {
  let raw: string
  try {
    const result = (await spawn('gh', ['api', apiPath], {
      stdio: 'pipe',
      stdioString: true,
    })) as { stdout?: string | undefined }
    raw = String(result?.stdout ?? '').trim()
  } catch (e) {
    return spawnFailureText(e).includes('404')
      ? { status: 'gone' }
      : { status: 'unavailable' }
  }
  try {
    return { payload: JSON.parse(raw) as GitRefPayload, status: 'ok' }
  } catch {
    return { status: 'unavailable' }
  }
}

export interface ResolvedTag {
  sha?: string | undefined
  status: 'gone' | 'resolved' | 'unavailable'
}

/**
 * The sha `pin`'s label resolves to today. A lightweight tag answers in one
 * lookup; an annotated tag answers with a tag OBJECT, whose sha is
 * dereferenced exactly once via `git/tags/<sha>` to reach the commit the
 * label actually names.
 */
export async function resolveTagSha(
  pin: TagPin,
  resolver: RefResolver,
): Promise<ResolvedTag> {
  const refLookup = await resolver(
    `repos/${pin.owner}/${pin.repo}/git/ref/tags/${pin.label}`,
  )
  if (refLookup.status === 'gone') {
    return { status: 'gone' }
  }
  if (refLookup.status === 'unavailable') {
    return { status: 'unavailable' }
  }
  const objectSha = refLookup.payload?.object?.sha ?? ''
  if (objectSha === '') {
    return { status: 'unavailable' }
  }
  if (refLookup.payload?.object?.type !== 'tag') {
    return { sha: objectSha, status: 'resolved' }
  }
  const tagLookup = await resolver(
    `repos/${pin.owner}/${pin.repo}/git/tags/${objectSha}`,
  )
  if (tagLookup.status !== 'ok') {
    return { status: 'unavailable' }
  }
  const commitSha = tagLookup.payload?.object?.sha ?? ''
  return commitSha === ''
    ? { status: 'unavailable' }
    : { sha: commitSha, status: 'resolved' }
}

export interface PinVerdict {
  currentSha?: string | undefined
  pin: TagPin
  verdict: 'gone' | 'match' | 'mismatch' | 'unverified'
}

/**
 * One pin's verdict: `match` when the label still resolves to the pinned
 * sha, `mismatch` when it resolves elsewhere, `gone` when the tag no longer
 * exists, `unverified` when no confident reading arrived.
 */
export async function judgePin(
  pin: TagPin,
  resolver: RefResolver,
): Promise<PinVerdict> {
  const resolved = await resolveTagSha(pin, resolver)
  if (resolved.status === 'gone') {
    return { pin, verdict: 'gone' }
  }
  if (resolved.status === 'unavailable') {
    return { pin, verdict: 'unverified' }
  }
  return {
    currentSha: resolved.sha,
    pin,
    verdict: resolved.sha === pin.sha ? 'match' : 'mismatch',
  }
}

/**
 * True for a generated artifact this check must not gate: a gh-aw compiled
 * `*.lock.yml` is rewritten wholesale by its compiler and is not ours to pin.
 */
export function isGeneratedSource(relPath: string): boolean {
  return relPath.endsWith('.lock.yml')
}

/**
 * Tracked YAML and Dockerfile sources. Tracked-only keeps build output and
 * vendored trees out of the scan.
 */
export function listPinSources(repoRoot: string): string[] {
  const result = spawnSync(
    'git',
    ['ls-files', '*.yml', '*.yaml', 'Dockerfile*', '*.Dockerfile'],
    { cwd: repoRoot, stdio: 'pipe', stdioString: true },
  )
  if (result.status !== 0) {
    return []
  }
  return String(result.stdout ?? '')
    .split(/\r?\n/)
    .map(s => s.trim())
    .filter(rel => rel.length > 0 && !isGeneratedSource(rel))
}

/**
 * True when the `gh` binary answers at all. Probed once up front so an
 * environment without the CLI skips loudly instead of failing per pin.
 */
async function isGhAvailable(): Promise<boolean> {
  try {
    await spawn('gh', ['--version'], { stdio: 'pipe', stdioString: true })
    return true
  } catch {
    return false
  }
}

export async function main(): Promise<void> {
  const quiet = process.argv.includes('--quiet')
  const sources = listPinSources(REPO_ROOT)
  const pins: TagPin[] = []
  const unresolvable: UnresolvablePin[] = []
  for (let i = 0, { length } = sources; i < length; i += 1) {
    const rel = sources[i]!
    let text: string
    try {
      text = readFileSync(path.join(REPO_ROOT, rel), 'utf8')
    } catch {
      continue
    }
    const scan = scanShaPins(text, rel)
    pins.push(...scan.pins)
    unresolvable.push(...scan.unresolvable)
  }
  for (let i = 0, { length } = unresolvable; i < length; i += 1) {
    const u = unresolvable[i]!
    logger.warn(
      `[pinned-labels-match-shas] unresolvable: ${u.file}:${u.line} pins ${u.sha} as ${u.label} with no <owner>/<repo> on the line, so the label cannot be verified.`,
    )
  }
  if (pins.length === 0) {
    if (!quiet) {
      logger.success(
        `[pinned-labels-match-shas] no resolvable tag-label pins to verify across ${sources.length} source(s); ${unresolvable.length} unresolvable.`,
      )
    }
    process.exitCode = 0
    return
  }
  if (!(await isGhAvailable())) {
    logger.warn(
      '[pinned-labels-match-shas] SKIPPED - `gh` is unavailable.\n' +
        `  ${pins.length} pinned label(s) were NOT verified this run.\n` +
        '  Fix: install and authenticate the GitHub CLI; CI and the release tier carry this check.',
    )
    process.exitCode = 0
    return
  }
  const verdicts: PinVerdict[] = []
  for (let i = 0, { length } = pins; i < length; i += 1) {
    verdicts.push(await judgePin(pins[i]!, ghRefResolver))
  }
  const unverified = verdicts.filter(v => v.verdict === 'unverified')
  const failures = verdicts.filter(
    v => v.verdict === 'gone' || v.verdict === 'mismatch',
  )
  for (let i = 0, { length } = unverified; i < length; i += 1) {
    const v = unverified[i]!
    logger.warn(
      `[pinned-labels-match-shas] UNVERIFIED ${v.pin.owner}/${v.pin.repo}@${v.pin.label} at ${v.pin.file}:${v.pin.line} - no confident answer from the API.`,
    )
  }
  if (failures.length === 0) {
    if (!quiet) {
      logger.success(
        `[pinned-labels-match-shas] ${verdicts.length - unverified.length} pinned label(s) still match their sha; ${unverified.length} unverified; ${unresolvable.length} unresolvable.`,
      )
    }
    process.exitCode = 0
    return
  }
  logger.fail(
    `[pinned-labels-match-shas] ${failures.length} pinned label(s) no longer match upstream:`,
  )
  logger.error(
    '  What:   a tag label moved or vanished out from under its pinned sha, so the comment now claims a version the tag no longer names.',
  )
  logger.error('  Where:  each pin listed below.')
  for (let i = 0, { length } = failures; i < length; i += 1) {
    const v = failures[i]!
    const current =
      v.verdict === 'gone' ? 'the tag is GONE' : `current ${v.currentSha}`
    logger.substep(
      `${v.pin.file}:${v.pin.line}  ${v.pin.owner}/${v.pin.repo}@${v.pin.label} pinned ${v.pin.sha}, ${current}`,
    )
  }
  logger.error(
    '  Saw:    the pinned sha beside the sha the label resolves to today, or no tag at all.',
  )
  logger.error(
    '  Fix:    treat a moved tag as a supply-chain signal. Verify upstream intent first - release notes, advisories, the tag history - and only repin, sha and label together, once the move is confirmed legitimate.',
  )
  process.exitCode = 1
}

const SCRIPT_META: ScriptMeta = {
  describe:
    'checks every `<sha> # v<label>` pin still resolves to the sha its upstream tag names',
  help: `Usage: node scripts/fleet/check/pinned-labels-match-shas.mts [flags]

  --quiet  suppress the pass message`,
}

/* c8 ignore start - entrypoint guard; exercised via subprocess. */
if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
/* c8 ignore stop */
