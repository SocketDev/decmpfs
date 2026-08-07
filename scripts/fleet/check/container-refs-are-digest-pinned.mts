#!/usr/bin/env node
/*
 * @file `check --all` gate: every hand-written container reference names
 *   immutable content. A registry TAG is a mutable pointer — re-pushing moves
 *   it, so the same reference serves different bytes tomorrow. Two forms are
 *   accepted:
 *
 *   1. A DIGEST pin, `name@sha256:<64 hex>`. The digest is the hash of the
 *      manifest, so it cannot lie about what it resolves to.
 *   2. A CONTENT-ADDRESSED TAG, whose text embeds a hex content hash of 7 or
 *      more characters. `fleet-pack-<template-sha>` is the fleet's own
 *      instance: different content yields a different sha, therefore a
 *      different tag, so the tag is content-unique by construction.
 *
 *   This is the container twin of the `uses:` SHA pinning the fleet already
 *   requires for GitHub Actions. Same threat, same answer: a mutable reference
 *   means the thing you audited is not necessarily the thing that runs.
 *
 *   Pinning alone is not the whole guarantee, and is not meant to be. The
 *   fleet's fetcher verifies every unpacked file's sha256 against the bundle
 *   manifest, so a repointed reference serving different bytes fails the hash
 *   check rather than applying silently. Reference immutable content AND verify
 *   integrity hashes on unpack; this check owns the first half.
 *
 *   Parsing is deliberately string-based rather than one regex. A pattern that
 *   matches a registry host needs nested quantifiers, which is a catastrophic
 *   backtracking hazard on adversarial input, and a lint scanner reading
 *   untrusted workflow text is exactly where that must not exist.
 *
 *   Scope: TRACKED, hand-written workflow and Dockerfile sources. Generated
 *   artifacts are skipped, `*.lock.yml` above all: a gh-aw compiled lock
 *   carries the firewall's own image references, is rewritten wholesale by its
 *   compiler, and is not ours to pin.
 *
 *   Exit: 0 — every reference is immutable, or none exist; 1 — a mutable tag
 *   reference is present.
 *
 *   Usage: node scripts/fleet/check/container-refs-are-digest-pinned.mts [--quiet]
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

const DIGEST_MARKER = '@sha256:'
const MIN_CONTENT_HASH_LENGTH = 7

export interface MutableRef {
  file: string
  // `mutable`: a plain tag, the reference can move. `unlabeled`: a bare
  // digest with no inline tag - immutable, but unreadable to a human, and a
  // Dockerfile FROM line cannot take a trailing comment, so the label must
  // ride inline as `name:tag@sha256:...`.
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

/**
 * True when a tag names immutable content: its final `-`, `_`, or `.` separated
 * segment is a hex hash of {@link MIN_CONTENT_HASH_LENGTH} or more characters.
 * `fleet-pack-7db5bc45…` passes; `v1.2.3` and `latest` do not. Pure; exported
 * for tests.
 */
export function isContentAddressedTag(tag: string): boolean {
  let start = -1
  for (let i = tag.length - 1; i >= 0; i -= 1) {
    const c = tag[i]!
    if (c === '_' || c === '-' || c === '.') {
      start = i + 1
      break
    }
  }
  const segment = start === -1 ? tag : tag.slice(start)
  return segment.length >= MIN_CONTENT_HASH_LENGTH && isHex(segment)
}

/**
 * The container reference in `token`, or undefined when it is not one. A
 * reference needs a registry HOST, recognized by a dot before the first slash,
 * so a bare local image name like `node:22` is never mistaken for one.
 * Returns the reference text and whether it already names immutable content.
 */
export function parseImageRef(
  token: string,
): { immutable: boolean; labeled: boolean; ref: string } | undefined {
  // Strip surrounding quotes and trailing punctuation a YAML or Dockerfile line
  // leaves attached.
  let text = token
  while (
    text.length &&
    (text[0] === "'" || text[0] === '"' || text[0] === '`')
  ) {
    text = text.slice(1)
  }
  while (
    text.length &&
    ('"\'`,;'.includes(text[text.length - 1]!) || text.endsWith('\\'))
  ) {
    text = text.slice(0, -1)
  }
  const firstSlash = text.indexOf('/')
  if (firstSlash <= 0) {
    return undefined
  }
  const host = text.slice(0, firstSlash)
  if (!host.includes('.')) {
    return undefined
  }
  const digestAt = text.indexOf(DIGEST_MARKER)
  if (digestAt !== -1) {
    // Immutable either way. LABELED only when a tag rides inline between the
    // final path segment and the digest (`name:tag@sha256:...`), the one form
    // a Dockerfile FROM line can carry, since it takes no trailing comment.
    const beforeDigest = text.slice(0, digestAt)
    const lastSlash = beforeDigest.lastIndexOf('/')
    const labeled = beforeDigest.indexOf(':', lastSlash) !== -1
    return { immutable: true, labeled, ref: text }
  }
  const lastSlash = text.lastIndexOf('/')
  const colon = text.indexOf(':', lastSlash)
  if (colon === -1) {
    // No tag at all. `name` with no tag resolves to `latest`, which is mutable,
    // but it is also how a bare repository path appears in prose and in a URL,
    // so it is left to other checks rather than guessed at here.
    return undefined
  }
  const tag = text.slice(colon + 1)
  if (!tag) {
    return undefined
  }
  // A content-addressed tag is its own label: the name IS the address.
  return { immutable: isContentAddressedTag(tag), labeled: true, ref: text }
}

/**
 * Every violating container reference in `text`: mutable tags, and bare
 * digests carrying no inline tag label. Pure; exported for tests.
 */
export function findMutableRefs(text: string, file: string): MutableRef[] {
  const out: MutableRef[] = []
  const lines = text.split(/\r?\n/)
  for (let i = 0, { length } = lines; i < length; i += 1) {
    const tokens = lines[i]!.split(/\s+/)
    for (let j = 0, { length: tokenCount } = tokens; j < tokenCount; j += 1) {
      const parsed = parseImageRef(tokens[j]!)
      if (!parsed) {
        continue
      }
      if (!parsed.immutable) {
        out.push({ file, kind: 'mutable', line: i + 1, ref: parsed.ref })
      } else if (!parsed.labeled) {
        out.push({ file, kind: 'unlabeled', line: i + 1, ref: parsed.ref })
      }
    }
  }
  return out
}

/**
 * True when a path is a GENERATED artifact this check must not gate. A gh-aw
 * `*.lock.yml` is the motivating case: its image references belong to the
 * firewall bundle its compiler emits, and pinning them by hand would be
 * overwritten on the next compile.
 */
export function isGeneratedSource(relPath: string): boolean {
  return relPath.endsWith('.lock.yml')
}

/**
 * Tracked workflow and Dockerfile sources. Tracked-only keeps build output and
 * vendored trees out of the scan.
 */
export function listContainerSources(repoRoot: string): string[] {
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

export function main(): number {
  const quiet = process.argv.includes('--quiet')
  const sources = listContainerSources(REPO_ROOT)
  const mutable: MutableRef[] = []
  for (let i = 0, { length } = sources; i < length; i += 1) {
    const rel = sources[i]!
    let text: string
    try {
      text = readFileSync(path.join(REPO_ROOT, rel), 'utf8')
    } catch {
      continue
    }
    mutable.push(...findMutableRefs(text, rel))
  }

  if (mutable.length === 0) {
    if (!quiet) {
      logger.success(
        `[container-refs-are-digest-pinned] every container reference across ${sources.length} source(s) names immutable content.`,
      )
    }
    return 0
  }

  const mutableKind = mutable.filter(m => m.kind === 'mutable')
  const unlabeled = mutable.filter(m => m.kind === 'unlabeled')
  logger.fail(
    '[container-refs-are-digest-pinned] container references violate the pin policy:',
  )
  if (mutableKind.length > 0) {
    logger.error(
      `  What:   ${mutableKind.length} reference(s) name a plain TAG. A tag is a mutable pointer, so the image audited today can serve different bytes tomorrow while the reference stays byte-identical.`,
    )
  }
  if (unlabeled.length > 0) {
    logger.error(
      `  What:   ${unlabeled.length} digest reference(s) carry no human label. The digest is immutable but unreadable; the tag must ride inline as \`name:tag${DIGEST_MARKER}...\`, because a Dockerfile FROM line cannot take a trailing comment.`,
    )
  }
  logger.error('  Where:  each reference listed below.')
  for (let i = 0, { length } = mutable; i < length; i += 1) {
    const ref = mutable[i]!
    logger.substep(`${ref.file}:${ref.line}  [${ref.kind}]  ${ref.ref}`)
  }
  logger.error(
    `  Saw:    a bare \`name:tag\`, or a bare \`name${DIGEST_MARKER}...\`; wanted \`name:tag${DIGEST_MARKER}<64 hex>\` (the digest pins, the inline tag labels), or a content-addressed tag whose final segment is the content hash.`,
  )
  logger.error(
    '  Fix:    resolve the digest once with `docker buildx imagetools inspect <ref>` and write `name:tag@sha256:...`. Keep verifying integrity hashes on unpack as well: pinning names the bytes, verification proves them.',
  )
  return 1
}

const SCRIPT_META: ScriptMeta = {
  describe:
    'every hand-written container reference is digest-pinned or content-addressed',
  help: `Usage: node scripts/fleet/check/container-refs-are-digest-pinned.mts [--quiet]`,
}

/* c8 ignore start - entry-point wiring, exercised by running the script. */
if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
/* c8 ignore stop */
