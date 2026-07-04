// Generate minimal placeholder packages to claim the npm names by hand.
//
// npm trusted publishing (OIDC) can only be configured on a package that already
// exists, and a brand-new name's first publish can't use OIDC — so these v0.0.0
// stubs are published manually (web auth) to claim every name (the main `decmpfs`
// package plus each @decmpfs/<triple>). Trusted publishing is then configured, and
// CI publishes the real binaries at the crate version with provenance.
//
// Placeholders carry no binary and OMIT `provenance` (a local publish has no OIDC
// token; only the CI workflow attests).

import { mkdirSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { TARGETS } from './targets.mts'

// Kept in lockstep with the crate description (crates/decmpfs/Cargo.toml).
const DESCRIPTION =
  'Apply OS-level transparent filesystem compression (APFS decmpfs / btrfs / NTFS) to a file in place.'
const REPOSITORY = 'https://github.com/decmpfs/decmpfs'

interface Placeholder {
  cpu?: string
  libc?: string
  name: string
  os?: string
  slug: string
}

const nodeRoot = join(dirname(fileURLToPath(import.meta.url)), '..')

// The main package first, then one per platform target.
const placeholders: Placeholder[] = [
  { name: 'decmpfs', slug: 'decmpfs' },
  ...TARGETS.map(target => ({
    cpu: target.cpu,
    libc: target.libc,
    name: `@decmpfs/${target.triple}`,
    os: target.os,
    slug: target.triple,
  })),
]

for (const placeholder of placeholders) {
  const dir = join(nodeRoot, 'placeholders', placeholder.slug)
  mkdirSync(dir, { recursive: true })

  const manifest = {
    name: placeholder.name,
    version: '0.0.0',
    description: DESCRIPTION,
    license: 'MIT',
    repository: REPOSITORY,
    ...(placeholder.os ? { os: [placeholder.os] } : {}),
    ...(placeholder.cpu ? { cpu: [placeholder.cpu] } : {}),
    ...(placeholder.libc ? { libc: [placeholder.libc] } : {}),
    files: ['README.md'],
    publishConfig: { access: 'public' },
  }
  writeFileSync(
    join(dir, 'package.json'),
    `${JSON.stringify(manifest, undefined, 2)}\n`,
  )
  writeFileSync(join(dir, 'README.md'), `# ${placeholder.name}\n\n${DESCRIPTION}\n`)
}
