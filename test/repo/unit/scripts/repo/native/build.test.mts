import { describe, expect, test } from 'vitest'

import { TARGETS } from '../../../../../../napi/decmpfs/scripts/targets.mts'
import {
  createNativeBuildPlan,
  nodeLibraryUrl,
  readRustToolchain,
  resolveNativeTarget,
} from '../../../../../../scripts/repo/native/build.mts'

describe('decmpfs native npm build', () => {
  test('resolves each configured npm target to its Rust target', () => {
    for (const target of TARGETS) {
      expect(resolveNativeTarget(target.triple)).toEqual(target)
    }
    expect(() => resolveNativeTarget('linux-x64')).toThrow()
  })

  test('uses the repository toolchain pin', () => {
    expect(readRustToolchain()).toBe('nightly-2026-10-04')
  })

  test('installs the target toolchain before the native build script', () => {
    const target = resolveNativeTarget('darwin-arm64')
    const plan = createNativeBuildPlan(target, {
      root: '/repo',
      toolchain: 'nightly-pinned',
      cargoRegistryPath: '/cargo/registry',
    })

    expect(plan.commands.map(command => command.command)).toEqual([
      'rustup',
      'rustup',
      process.execPath,
    ])
    expect(plan.commands[0]?.args).toEqual([
      'toolchain',
      'install',
      'nightly-pinned',
      '--profile',
      'minimal',
    ])
    expect(plan.commands[1]?.args).toEqual([
      'target',
      'add',
      '--toolchain',
      'nightly-pinned',
      'aarch64-apple-darwin',
    ])
    expect(plan.commands[2]).toMatchObject({
      args: [
        '/repo/napi/decmpfs/scripts/build.mts',
        '--prod',
        '--target',
        'aarch64-apple-darwin',
      ],
      cwd: '/repo/napi/decmpfs',
      env: { RUSTUP_TOOLCHAIN: 'nightly-pinned' },
    })
    expect(plan.artifactPath).toBe('/repo/napi/decmpfs/decmpfs.node')
    expect(plan.alpineLoadCheck).toBe(false)
    expect(plan.nativeLoadCheck).toBe(true)
  })

  test('builds musl targets in pinned Alpine with a matching gcc linker', () => {
    const target = resolveNativeTarget('linux-arm64-musl')
    const plan = createNativeBuildPlan(target, {
      root: '/repo',
      toolchain: 'nightly-pinned',
      cargoRegistryPath: '/cargo/registry',
    })
    const command = plan.commands[0]

    expect(command?.command).toBe('docker')
    expect(command?.args).toContain(
      'rust:alpine@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627',
    )
    expect(command?.args).toContain(
      'CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=gcc',
    )
    expect(command?.args.at(-1)).toContain(
      'rustup target add --toolchain "$RUST_TOOLCHAIN" "$RUST_TARGET"',
    )
    expect(plan.artifactPath).toBe(
      '/repo/target/aarch64-unknown-linux-musl/release/libdecmpfs_node.so',
    )
    expect(plan.alpineLoadCheck).toBe(true)
    expect(plan.nativeLoadCheck).toBe(false)
  })

  test('downloads the node.lib that matches each Windows target and Node version', () => {
    expect(
      nodeLibraryUrl(resolveNativeTarget('win32-arm64-msvc'), 'v26.9.0'),
    ).toBe('https://nodejs.org/dist/v26.9.0/win-arm64/node.lib')
  })
})
