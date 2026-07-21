'use strict'

const { Writable } = require('node:stream')

// Resolve the prebuilt addon for this host. Each supported target ships as an
// optional dependency (@decmpfs/<triple>) carrying only its `.node`, so pnpm
// installs just the one matching this platform/arch/abi. Prefer a local
// `./decmpfs.node` from `pnpm run build` so development never exercises a stale
// installed prebuild; published packages fall through to the platform package.

const { arch, platform } = process

// The napi-rs addon abi suffix: glibc Linux is `-gnu`, musl Linux is `-musl`,
// Windows is `-msvc`, macOS none. (Keep in lockstep with scripts/make-npm-dirs.mts.)
function abiSuffix() {
  if (platform === 'win32') {
    return '-msvc'
  }
  if (platform === 'linux') {
    const report = process.report?.getReport()
    const glibc =
      report && typeof report === 'object'
        ? report.header?.glibcVersionRuntime
        : undefined
    return glibc ? '-gnu' : '-musl'
  }
  return ''
}

const triple = `${platform}-${arch}${abiSuffix()}`
const platformPackage = `@decmpfs/${triple}`

// Hoisted so it sits in visibility-group order beside the other module
// helpers; it constructs the DecmpfsWriteStream class defined below (only
// invoked at runtime, long after the class is evaluated).
function createDecmpfsWriteStream(path, options) {
  return new DecmpfsWriteStream(path, options)
}

function load() {
  try {
    return require('./decmpfs.node')
  } catch {}
  try {
    return require(platformPackage)
  } catch {}
  throw new Error(
    `decmpfs: no prebuilt binary for ${triple}. Install the optional dependency ` +
      `${platformPackage}, or build from source with \`pnpm run build\`.`,
  )
}

const binding = load()

class DecmpfsWriteStream extends Writable {
  constructor(path, options) {
    super()
    const { size, ...writeOptions } = options || {}
    if (!Number.isSafeInteger(size) || size < 0) {
      throw new TypeError(
        'decmpfs stream size must be a non-negative safe integer',
      )
    }
    this.handle = new binding.DecmpfsWriteHandle(path, size, writeOptions)
    this.result = undefined
  }

  // oxlint-disable-next-line socket/no-underscore-identifier -- `_destroy` is the node:stream Writable override method; its name is fixed by the Node streams API.
  _destroy(error, callback) {
    let finalError = error
    if (this.handle) {
      try {
        this.handle.abort()
      } catch (abortError) {
        finalError ||= abortError
      }
      this.handle = undefined
    }
    callback(finalError)
  }

  // oxlint-disable-next-line socket/no-underscore-identifier -- `_final` is the node:stream Writable override method; its name is fixed by the Node streams API.
  _final(callback) {
    try {
      this.result = this.handle.finish()
      this.handle = undefined
      callback()
    } catch (error) {
      callback(error)
    }
  }

  // oxlint-disable-next-line socket/no-underscore-identifier -- `_write` is the node:stream Writable override method; its name is fixed by the Node streams API.
  _write(chunk, _encoding, callback) {
    try {
      this.handle.write(chunk)
      callback()
    } catch (error) {
      callback(error)
    }
  }
}

binding.DecmpfsWriteStream = DecmpfsWriteStream
binding.createDecmpfsWriteStream = createDecmpfsWriteStream

module.exports = binding
