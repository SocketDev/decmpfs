'use strict'

// Resolve the prebuilt addon for this host. Each supported target ships as an
// optional dependency (@jdalton/decmpfs-<triple>) carrying only its `.node`, so
// npm installs just the one matching this platform/arch/abi. A local
// `./decmpfs.node` (from `npm run build`) is the dev fallback.

const { arch, platform } = process

// The ABI suffix napi-style triples use: glibc/musl on Linux, the MSVC ABI on
// Windows, nothing on macOS.
function abiSuffix() {
  if (platform === 'win32') {
    return '-msvc'
  }
  if (platform === 'linux') {
    const report = process.report?.getReport()
    const glibc =
      report && typeof report === 'object' ? report.header?.glibcVersionRuntime : undefined
    return glibc ? '-gnu' : '-musl'
  }
  return ''
}

const triple = `${platform}-${arch}${abiSuffix()}`
const platformPackage = `@jdalton/decmpfs-${triple}`

function load() {
  try {
    return require(platformPackage)
  } catch {}
  try {
    return require('./decmpfs.node')
  } catch {}
  throw new Error(
    `decmpfs: no prebuilt binary for ${triple}. Install the optional dependency ` +
      `${platformPackage}, or build from source with \`npm run build\`.`,
  )
}

module.exports = load()
