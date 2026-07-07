// Cut a decmpfs release: bump the version in lockstep across the crate + npm
// manifests, add a CHANGELOG section, commit `chore: bump version to X.Y.Z`,
// and tag vX.Y.Z. The tag push (do it yourself once you've reviewed) fires
// github-release.yml → the GitHub Release → publish-crate.yml + publish-npm.yml.
//
//   node scripts/release.mjs 0.3.0        # bump, changelog stub, commit, tag
//   node scripts/release.mjs 0.3.0 --push # also push main + the tag
//
// The version lands in: crates/decmpfs/Cargo.toml [package], Cargo.lock (the
// decmpfs entry), napi/decmpfs/package.json (version + every @decmpfs/* pin).
// A fresh `## X.Y.Z` CHANGELOG section is inserted with a TODO bullet for you
// to fill before pushing — check-versions + the changelog gate run first.

import { execFileSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..')
const version = (process.argv[2] ?? '').replace(/^v/, '')
const push = process.argv.includes('--push')

function die(msg) {
  process.stderr.write(`release: ${msg}\n`)
  process.exit(1)
}

if (!/^\d+\.\d+\.\d+$/.test(version)) {
  die(
    `usage: node scripts/release.mjs <x.y.z> [--push]\n` +
      `  where: argv[2]. saw: ${JSON.stringify(process.argv[2])}. ` +
      `fix: pass a semver, e.g. 0.3.0.`,
  )
}

function edit(rel, fn) {
  const p = path.join(root, rel)
  const before = readFileSync(p, 'utf8')
  const after = fn(before)
  if (after === before) {
    die(`no change written to ${rel} — expected a version edit. Fix: check the file's shape.`)
  }
  writeFileSync(p, after)
}

// crates/decmpfs/Cargo.toml — the FIRST top-level `version = "…"` ([package]),
// never a nested dependency pin.
edit('crates/decmpfs/Cargo.toml', src =>
  src.replace(/^version\s*=\s*"[^"]+"/m, `version = "${version}"`),
)

// napi/decmpfs/package.json — the package version + every @decmpfs/* pin.
edit('napi/decmpfs/package.json', src => {
  const pkg = JSON.parse(src)
  pkg.version = version
  for (const name of Object.keys(pkg.optionalDependencies ?? {})) {
    if (name.startsWith('@decmpfs/')) {
      pkg.optionalDependencies[name] = version
    }
  }
  return JSON.stringify(pkg, null, 2) + '\n'
})

// Cargo.lock — refresh the decmpfs entry (and anything that pins it) with a
// lockfile-only update so the lock stays consistent without touching deps.
execFileSync('cargo', ['update', '--offline', '-p', 'decmpfs', '--precise', version], {
  cwd: root,
  stdio: 'inherit',
})

// CHANGELOG.md — insert a fresh section above the newest existing one, with a
// TODO bullet to fill before pushing (a real release must not ship the stub).
edit('CHANGELOG.md', src => {
  if (src.includes(`## ${version}`)) {
    return src // already present — an amended re-run
  }
  return src.replace(
    /\n## /,
    `\n## ${version}\n\n- TODO: describe the user-visible changes in this release.\n\n## `,
  )
})

// Gate: crate + npm + pins must agree before we commit the bump.
execFileSync(process.execPath, [path.join(root, 'scripts', 'check-versions.mjs')], {
  cwd: root,
  stdio: 'inherit',
})

execFileSync(
  'git',
  [
    'commit',
    '-o',
    'crates/decmpfs/Cargo.toml',
    'napi/decmpfs/package.json',
    'Cargo.lock',
    'CHANGELOG.md',
    '-m',
    `chore: bump version to ${version}`,
  ],
  { cwd: root, stdio: 'inherit' },
)
execFileSync('git', ['tag', `v${version}`], { cwd: root, stdio: 'inherit' })

if (push) {
  const branch = execFileSync('git', ['symbolic-ref', '--short', 'HEAD'], {
    cwd: root,
    encoding: 'utf8',
  }).trim()
  execFileSync('git', ['push', 'origin', branch], { cwd: root, stdio: 'inherit' })
  execFileSync('git', ['push', 'origin', `v${version}`], { cwd: root, stdio: 'inherit' })
}

console.log(
  `release: committed + tagged v${version}.` +
    (push
      ? ' Pushed — github-release.yml will cut the Release and publish.'
      : ' Fill the CHANGELOG section, then `git push origin HEAD --tags` to release.'),
)
