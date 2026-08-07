/**
 * @file The ONE place that answers "where does this repo's release version
 *   live?" Every consumer reads this rather than re-deriving it, so the release
 *   pipeline, a build stamping a version into a binary, and the private-package
 *   gate can never disagree. Most packages take their version from npm, because
 *   the package publishes and so its manifest `version` IS the released
 *   version. A few repos do not work that way, and naming that SPECIAL case is
 *   why this module exists. A `custom` channel repo ships to a marketplace. Its
 *   root manifest stays `private: true` because nothing goes to npm, while
 *   `version` is the number the VS Code and Open VSX listings receive. A
 *   `binary` channel repo ships GitHub release assets, and its root manifest
 *   carries the release version even though no npm package exists. A workspace
 *   repo designates one manifest as the version source, and the lockstep bump
 *   moves every member to match it. In all three the manifest is private and
 *   versioned, which looks exactly like the mistake
 *   `private-packages-are-unpublishable` exists to catch. The difference cannot
 *   be inferred from the file, because `private: true` beside a real version is
 *   the same bytes either way. So it is DECLARED per repo, as
 *   `release.versionSource` in `.config/repo/socket-wheelhouse.json`, holding
 *   the repo-relative path of the manifest that owns the release version.
 *   Declaring beats inferring "the root is probably fine" three ways: the
 *   exemption becomes visible in review, a repo that does not need one is never
 *   silently granted it, and a build asking "what version am I stamping?" has a
 *   single answer instead of a per-build guess.
 */

import { existsSync, readFileSync } from 'node:fs'
import path from 'node:path'

import { normalizePath } from '@socketsecurity/lib-stable/paths/normalize'

const REPO_CONFIG_REL = '.config/repo/socket-wheelhouse.json'

export interface ReleaseVersionSource {
  // Repo-relative POSIX path of the manifest that holds the release version.
  relPath: string
  // The version it currently carries, or undefined when the file is missing or
  // carries no `version` field.
  version: string | undefined
}

/**
 * The declared release version source, or undefined when the repo declares
 * none. Undefined is a real answer: a repo whose packages all publish to npm
 * needs no designated source, because each manifest's own version is its
 * released version.
 */
export function readVersionSource(
  repoRoot: string,
): ReleaseVersionSource | undefined {
  let declared: unknown
  try {
    const cfg = JSON.parse(
      readFileSync(path.join(repoRoot, REPO_CONFIG_REL), 'utf8'),
    ) as { release?: { versionSource?: unknown | undefined } | undefined }
    declared = cfg.release?.versionSource
  } catch {
    return undefined
  }
  if (typeof declared !== 'string' || !declared) {
    return undefined
  }
  const relPath = normalizePath(declared)
  const abs = path.join(repoRoot, relPath)
  if (!existsSync(abs)) {
    // A declared-but-missing source is a finding for the check that validates
    // it, not something to paper over with a guess here.
    return { relPath, version: undefined }
  }
  let version: string | undefined
  try {
    const manifest = JSON.parse(readFileSync(abs, 'utf8')) as {
      version?: unknown | undefined
    }
    if (typeof manifest.version === 'string') {
      version = manifest.version
    }
  } catch {
    version = undefined
  }
  return { relPath, version }
}

/**
 * True when `manifestRelPath` is the repo's declared release version source.
 * Paths are normalized on both sides so a Windows-separated path matches its
 * POSIX declaration — a missed match would strip the exemption and force a
 * released version to 0.0.0.
 */
export function isVersionSourceManifest(
  manifestRelPath: string,
  source: ReleaseVersionSource | undefined,
): boolean {
  if (!source) {
    return false
  }
  return normalizePath(manifestRelPath) === normalizePath(source.relPath)
}
