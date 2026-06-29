// Generate minimal placeholder packages for each @decmpfs/<triple> under
// placeholders/<triple>/, so they can be published once to claim the names. npm
// trusted publishing (OIDC) can only be configured on a package that already
// exists, and the first publish of a brand-new name can't use OIDC — so these are
// published manually (web auth), then trusted publishing is set up, then CI
// publishes the real binaries with provenance.
//
// Placeholders are version 0.0.0, carry no binary, and intentionally OMIT
// `provenance` (a local publish has no OIDC token; only the CI workflow attests).

import { mkdirSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { TARGETS } from './targets.mts'

const nodeRoot = join(dirname(fileURLToPath(import.meta.url)), '..')

for (const target of TARGETS) {
  const name = `@decmpfs/${target.triple}`
  const dir = join(nodeRoot, 'placeholders', target.triple)
  mkdirSync(dir, { recursive: true })

  const manifest = {
    name,
    version: '0.0.0',
    description: `Placeholder for the decmpfs ${target.triple} prebuilt addon — the real binary is published by CI.`,
    license: 'MIT',
    repository: 'https://github.com/jdalton/decmpfs',
    os: [target.os],
    cpu: [target.cpu],
    ...(target.libc ? { libc: [target.libc] } : {}),
    publishConfig: { access: 'public' },
  }
  writeFileSync(join(dir, 'package.json'), `${JSON.stringify(manifest, undefined, 2)}\n`)
  writeFileSync(
    join(dir, 'README.md'),
    `# ${name}\n\nPlaceholder. The real \`${target.triple}\` prebuilt addon for ` +
      `[decmpfs](https://github.com/jdalton/decmpfs) is published by CI.\n`,
  )
}
