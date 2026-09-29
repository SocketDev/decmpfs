// Canonical list of platforms we ship a prebuilt `.node` for — the single source
// of truth for the loader's package names, the main package's optionalDependencies,
// and the per-triple npm directories the publish workflow builds.
//
// These are ADDONS (a `.node` dlopen'd into Node), not standalone binaries, so the
// triple follows napi-rs's `platformArchABI` (`<platform>-<arch>[-<abi>]`) where the
// abi is explicit: glibc Linux is `-gnu`, musl Linux is `-musl`, Windows is `-msvc`.
// The abi must be in the name because an addon built against one C library cannot
// load on another, and glibc/musl hosts genuinely coexist. `libc` is also set as the
// npm install gate.

export interface Target {
  triple: string
  os: string
  cpu: string
  // The npm install gate: Linux glibc vs musl.
  libc?: string | undefined
  // The full Rust target triple the toolchain + cross builds take.
  rust: string
  // The cdylib basename cargo emits on this target's native host.
  artifact: string
}

export const TARGETS: Target[] = [
  {
    triple: 'darwin-arm64',
    os: 'darwin',
    cpu: 'arm64',
    rust: 'aarch64-apple-darwin',
    artifact: 'libdecmpfs_node.dylib',
  },
  {
    triple: 'darwin-x64',
    os: 'darwin',
    cpu: 'x64',
    rust: 'x86_64-apple-darwin',
    artifact: 'libdecmpfs_node.dylib',
  },
  {
    triple: 'linux-arm64-gnu',
    os: 'linux',
    cpu: 'arm64',
    libc: 'glibc',
    rust: 'aarch64-unknown-linux-gnu',
    artifact: 'libdecmpfs_node.so',
  },
  {
    triple: 'linux-x64-gnu',
    os: 'linux',
    cpu: 'x64',
    libc: 'glibc',
    rust: 'x86_64-unknown-linux-gnu',
    artifact: 'libdecmpfs_node.so',
  },
  {
    triple: 'win32-x64-msvc',
    os: 'win32',
    cpu: 'x64',
    rust: 'x86_64-pc-windows-msvc',
    artifact: 'decmpfs_node.dll',
  },
  {
    triple: 'linux-arm64-musl',
    os: 'linux',
    cpu: 'arm64',
    libc: 'musl',
    rust: 'aarch64-unknown-linux-musl',
    artifact: 'libdecmpfs_node.so',
  },
  {
    triple: 'linux-x64-musl',
    os: 'linux',
    cpu: 'x64',
    libc: 'musl',
    rust: 'x86_64-unknown-linux-musl',
    artifact: 'libdecmpfs_node.so',
  },
  {
    triple: 'win32-arm64-msvc',
    os: 'win32',
    cpu: 'arm64',
    rust: 'aarch64-pc-windows-msvc',
    artifact: 'decmpfs_node.dll',
  },
]
