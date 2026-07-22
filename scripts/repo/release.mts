// Cut a decmpfs release — the ONE command that tags and triggers publishing.
// You never tag by hand: this owns the tag, points it at the release commit,
// and (with --push) pushes it, which fires github-release.yml → the GitHub
// Release → publish-crate.yml + publish-npm.yml.
//
//   node scripts/repo/release.mts               # release the committed version
//   node scripts/repo/release.mts --dry-run     # preview: detected version + CHANGELOG, no writes
//   node scripts/repo/release.mts 0.3.0         # bump to 0.3.0 first, then release
//   node scripts/repo/release.mts [ver] --push  # also push branch + tag (triggers CI)
//
// A committed `-prerelease` version (e.g. 0.1.1-prerelease) auto-finalizes to the
// plain semver (0.1.1), reusing the CHANGELOG section already written for it.
//
// Two modes:
//   - No version arg (or a version equal to the current one): release WHAT IS
//     COMMITTED. No bump commit; the tag lands on HEAD. Use this for a version
//     already bumped in the manifests (and to debut the automation itself —
//     HEAD is where github-release.yml lives).
//   - A NEW version arg: bump the crate + npm manifests + Cargo.lock in
//     lockstep, insert a CHANGELOG section, and commit
//     `chore: bump version to X.Y.Z` before tagging.
//
// .mts like every decmpfs script; Node 24 (the repo baseline, .node-version)
// strips the types natively in CI + git hooks.

import { readFileSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

const logger = getDefaultLogger()

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
const argv = process.argv.slice(2).filter(a => !a.startsWith('--'))
const arg = (argv[0] ?? '').replace(/^v/, '')
const push = process.argv.includes('--push')
const dryRun = process.argv.includes('--dry-run')

// Resolve the version to release from the committed version and an optional arg:
//   - a `-prerelease` (or any `-suffix`) committed version + no arg → FINALIZE
//     to the plain semver (0.1.1-prerelease → 0.1.1), reusing the CHANGELOG
//     section already written for it (kept verbatim, never re-stubbed);
//   - a new semver arg → BUMP (insert a CHANGELOG stub to fill in);
//   - otherwise release WHAT IS COMMITTED.
function resolveRelease(current: string, argVersion: string): {
  version: string
  mode: 'finalize' | 'bump' | 'as-committed'
} {
  const pre = current.match(/^(?<base>\d+\.\d+\.\d+)-[0-9A-Za-z.-]+$/)
  if (pre && !argVersion) {
    return { version: pre.groups!['base']!, mode: 'finalize' }
  }
  if (argVersion && argVersion !== current) {
    return { version: argVersion, mode: 'bump' }
  }
  return { version: current, mode: 'as-committed' }
}

function die(msg: string): never {
  process.stderr.write(`release: ${msg}\n`)
  process.exit(1)
}

interface GitOptions {
  stdio?: 'inherit' | 'pipe' | undefined
}

function git(args: string[], options: GitOptions = {}): string {
  const result = spawnSync('git', args, {
    cwd: root,
    encoding: 'utf8',
    ...options,
  })
  if (result.status !== 0) {
    die(`git ${args[0]} exited ${result.status ?? 'on a signal'}.`)
  }
  return String(result.stdout ?? '')
}

function currentVersion(): string {
  const cargo = readFileSync(
    path.join(root, 'crates', 'decmpfs', 'Cargo.toml'),
    'utf8',
  )
  const captured = cargo.match(/^version\s*=\s*"(?<version>[^"]+)"/m)?.[1]
  if (captured === undefined) {
    die('no [package] version in crates/decmpfs/Cargo.toml.')
  }
  return captured
}

if (arg && !/^\d+\.\d+\.\d+$/.test(arg)) {
  die(
    `usage: node scripts/repo/release.mts [x.y.z] [--dry-run] [--push]\n` +
      `  saw: ${JSON.stringify(argv[0])}. fix: omit the arg to release the ` +
      `committed version, or pass a semver like 0.3.0 to bump first.`,
  )
}

// A release must reflect committed state — the tag points at a commit, so a
// dirty tree would tag something that was never tested. A dry run only reads, so
// it tolerates a dirty tree.
if (!dryRun && git(['status', '--porcelain']).trim()) {
  die('working tree is dirty. Fix: commit or stash before releasing.')
}

const current = currentVersion()
const { version, mode } = resolveRelease(current, arg)
// Finalize and bump both rewrite the manifests to the release version.
const bump = version !== current

function edit(rel: string, fn: (src: string) => string): void {
  const p = path.join(root, rel)
  const before = readFileSync(p, 'utf8')
  const after = fn(before)
  if (after === before) {
    die(
      `no change written to ${rel} — expected a version edit. Fix: check the file's shape.`,
    )
  }
  writeFileSync(p, after)
}

// Preview the plan and exit before touching anything.
if (dryRun) {
  const changelogHas = readFileSync(path.join(root, 'CHANGELOG.md'), 'utf8').includes(
    `## ${version}`,
  )
  logger.log(
    `release (dry-run):\n` +
      `  committed version: ${current}\n` +
      `  release version:   ${version}  (${mode})\n` +
      `  changelog:         ${changelogHas ? `## ${version} present — kept verbatim` : `## ${version} MISSING — a section must be written first`}\n` +
      `  manifests:         ${bump ? `rewrite Cargo.toml + napi/decmpfs/package.json + Cargo.lock to ${version}` : 'unchanged'}\n` +
      `  tag:               v${version} at HEAD\n` +
      `  push:              ${push ? 'yes → github-release.yml publishes' : 'no (re-run with --push)'}`,
  )
  process.exit(0)
}

if (bump) {
  edit('crates/decmpfs/Cargo.toml', src =>
    src.replace(/^version\s*=\s*"[^"]+"/m, `version = "${version}"`),
  )
  edit('napi/decmpfs/package.json', src => {
    const pkg = JSON.parse(src) as {
      version: string
      optionalDependencies?: Record<string, string> | undefined
    }
    pkg.version = version
    const optNames = Object.keys(pkg.optionalDependencies ?? {})
    for (let i = 0, { length } = optNames; i < length; i += 1) {
      const name = optNames[i]!
      if (name.startsWith('@decmpfs/')) {
        pkg.optionalDependencies![name] = version
      }
    }
    return JSON.stringify(pkg, null, 2) + '\n'
  })
  const updateResult = spawnSync(
    'cargo',
    ['update', '--offline', '-p', 'decmpfs', '--precise', version],
    {
      cwd: root,
      stdio: 'inherit',
    },
  )
  if (updateResult.status !== 0) {
    die(`cargo update exited ${updateResult.status ?? 'on a signal'}.`)
  }
  // The napi manifest version changed — resync pnpm-lock.yaml so a release never
  // leaves the lock drifted (a dirty lock blocks the next release and ships
  // stale `link:` specifiers).
  const relocked = spawnSync(
    'pnpm',
    ['install', '--lockfile-only', '--ignore-scripts'],
    { cwd: root, stdio: 'inherit' },
  )
  if (relocked.status !== 0) {
    die(`pnpm install --lockfile-only exited ${relocked.status ?? 'on a signal'}.`)
  }
  // Finalizing a prerelease keeps the section already written for this version;
  // only a fresh bump inserts a TODO stub to fill in. (The gate below still
  // requires a real, non-stub section before the release proceeds.)
  const changelog = readFileSync(path.join(root, 'CHANGELOG.md'), 'utf8')
  if (!changelog.includes(`## ${version}`)) {
    edit('CHANGELOG.md', src =>
      src.replace(
        /\n## /,
        `\n## ${version}\n\n- TODO: describe the user-visible changes in this release.\n\n## `,
      ),
    )
  }
}

// Every release path runs the lockstep gate + requires a real CHANGELOG section.
const gateResult = spawnSync(
  process.execPath,
  [path.join(root, 'scripts', 'repo', 'check-versions.mts')],
  {
    cwd: root,
    stdio: 'inherit',
  },
)
if (gateResult.status !== 0) {
  die(`version parity gate exited ${gateResult.status ?? 'on a signal'}.`)
}
const notes = String(
  spawnSync(
    process.execPath,
    [path.join(root, 'scripts', 'repo', 'changelog-section.mts'), version],
    { cwd: root, encoding: 'utf8' },
  ).stdout ?? '',
).trim()
if (!notes || /TODO: describe the user-visible changes/.test(notes)) {
  die(
    `CHANGELOG.md "## ${version}" section is missing or still a TODO stub. ` +
      `Fix: fill it in, commit, then re-run.`,
  )
}

if (bump) {
  const files = [
    'crates/decmpfs/Cargo.toml',
    'napi/decmpfs/package.json',
    'Cargo.lock',
    'CHANGELOG.md',
  ]
  // Include the lockfile only when the resync above actually changed it.
  if (git(['status', '--porcelain', 'pnpm-lock.yaml']).trim()) {
    files.push('pnpm-lock.yaml')
  }
  git(
    [
      'commit',
      '-o',
      ...files,
      '-m',
      `chore: bump version to ${version}`,
    ],
    { stdio: 'inherit' },
  )
}

// The script owns the tag: place it at the release commit (HEAD), moving a
// stale local tag forward if one exists. -f is safe for a tag not yet pushed;
// a published tag must never move (immutable release), so pushing below is
// non-forced and will reject a moved-after-publish tag loudly.
const tag = `v${version}`
git(['tag', '-f', tag], { stdio: 'inherit' })

if (push) {
  const branch = git(['symbolic-ref', '--short', 'HEAD']).trim()
  git(['push', 'origin', branch], { stdio: 'inherit' })
  git(['push', 'origin', tag], { stdio: 'inherit' })
}

logger.log(
  `release: ${bump ? `bumped + ` : ''}tagged ${tag} at HEAD.` +
    (push
      ? ` Pushed — github-release.yml cuts the Release, which publishes.`
      : ` Review, then: node scripts/repo/release.mts ${arg ? version + ' ' : ''}--push`),
)
