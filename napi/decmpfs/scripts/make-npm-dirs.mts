// Generate the per-triple npm package directories under napi/decmpfs/npm/<triple>/, one
// per TARGETS entry: a manifest gated by os/cpu/libc that ships only that
// platform's `.node`. Idempotent codegen — the publish workflow runs this on each
// matrix host, then copies the freshly built binary into its matching directory
// (this script also does that copy locally when a host build is present).

import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

import { TARGETS } from './targets.mts'

const nodeRoot = path.join(path.dirname(fileURLToPath(import.meta.url)), '..')
// This package is a member of the cargo workspace rooted at the repo, so cargo
// writes the cdylib to the WORKSPACE-ROOT target/, not this package's dir.
const repoRoot = path.join(nodeRoot, '..', '..')
const mainManifest = JSON.parse(
  readFileSync(path.join(nodeRoot, 'package.json'), 'utf8'),
)

// napi-rs addon naming, kept in lockstep with index.cjs: glibc Linux is `-gnu`,
// musl Linux is `-musl`, Windows is `-msvc`, macOS none.
function hostTriple(): string {
  const { arch, platform } = process
  if (platform === 'win32') {
    return `${platform}-${arch}-msvc`
  }
  if (platform === 'linux') {
    const report = process.report?.getReport()
    const glibc =
      report && typeof report === 'object'
        ? report.header?.glibcVersionRuntime
        : undefined
    return `${platform}-${arch}${glibc ? '-gnu' : '-musl'}`
  }
  return `${platform}-${arch}`
}

const host = hostTriple()

// The MIT license text every published package must ship (fleet convention:
// README + LICENSE in every publish). Read from the main addon package so the
// per-triple copies never drift from it.
const license = readFileSync(path.join(nodeRoot, 'LICENSE'), 'utf8')

for (const target of TARGETS) {
  const dir = path.join(nodeRoot, 'npm', target.triple)
  mkdirSync(dir, { recursive: true })

  const nodeFile = `decmpfs.${target.triple}.node`
  const manifest = {
    name: `@decmpfs/${target.triple}`,
    version: mainManifest.version,
    description: `decmpfs prebuilt binary for ${target.triple}.`,
    license: mainManifest.license,
    repository: mainManifest.repository,
    engines: mainManifest.engines,
    os: [target.os],
    cpu: [target.cpu],
    ...(target.libc ? { libc: [target.libc] } : {}),
    main: nodeFile,
    files: [nodeFile],
    publishConfig: mainManifest.publishConfig,
  }
  writeFileSync(
    path.join(dir, 'package.json'),
    `${JSON.stringify(manifest, undefined, 2)}\n`,
  )

  // Every published package ships a README + LICENSE (npm includes both without
  // a `files:` entry). The README points consumers at the `decmpfs` package.
  writeFileSync(
    path.join(dir, 'README.md'),
    `# @decmpfs/${target.triple}\n\n${manifest.description}\n\n` +
      'This is the prebuilt native binary for ' +
      '[decmpfs](https://www.npmjs.com/package/decmpfs). Install `decmpfs` ' +
      'instead — it depends on this package for your platform.\n',
  )
  writeFileSync(path.join(dir, 'LICENSE'), license)

  // On the matrix host, stage the binary cargo just built into its dir.
  if (target.triple === host) {
    const built = path.join(repoRoot, 'target', 'release', target.artifact)
    if (existsSync(built)) {
      copyFileSync(built, path.join(dir, nodeFile))
    }
  }
}
