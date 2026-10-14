import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { httpBytes } from '@socketsecurity/lib-stable/http-request'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

import { TARGETS } from '../../../napi/decmpfs/scripts/targets.mts'
import type { Target } from '../../../napi/decmpfs/scripts/targets.mts'
import { isMainModule } from '../../fleet/process/is-main-module.mts'
import { runMain } from '../../fleet/process/main/run.mts'
import type { ScriptMeta } from '../../fleet/process/main/run.mts'
import { REPO_CACHE_DIR } from '../../fleet/paths.mts'

const rootPath = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  '..',
)
const logger = getDefaultLogger()
const napiPath = path.join(rootPath, 'napi', 'decmpfs')
const RUST_IMAGE =
  'rust:alpine@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627'
const NODE_IMAGE =
  'node:24-alpine@sha256:ebfe2f90462722a7a4de65e91990e97fe0d401c70e0e762c5b53302f905ec1c1'
const SCRIPT_META: ScriptMeta = {
  describe: 'Build and stage one decmpfs native npm addon target.',
  help: 'Usage: node scripts/repo/native/build.mts <target> <output-dir> [--json]\nBuilds the target addon with the pinned Rust toolchain and stages decmpfs.node.',
  json: 'result',
}

export interface NativeBuildCommand {
  command: string
  args: string[]
  cwd: string
  env?: Record<string, string> | undefined
}

export interface NativeBuildPlan {
  commands: NativeBuildCommand[]
  artifactPath: string
  alpineLoadCheck: boolean
  nativeLoadCheck: boolean
}

export function resolveNativeTarget(targetName: string): Target {
  const target = TARGETS.find(candidate => candidate.triple === targetName)
  if (!target) {
    throw new Error(
      `native-build: target is unsupported. Where: napi/decmpfs/scripts/targets.mts. Saw: ${targetName}. Wanted: one configured Decmpfs addon target. Fix: pass a target from napi.platforms.`,
    )
  }
  return target
}

export function readRustToolchain(root = rootPath): string {
  const toolchainPath = path.join(root, 'rust-toolchain.toml')
  const channel = /^channel\s*=\s*"([^"]+)"/mu.exec(
    readFileSync(toolchainPath, 'utf8'),
  )?.[1]
  if (!channel) {
    throw new Error(
      `native-build: Rust toolchain is missing. Where: ${toolchainPath}. Saw: no channel. Wanted: a pinned rust-toolchain.toml channel. Fix: restore the channel field.`,
    )
  }
  return channel
}

export function nodeLibraryUrl(
  target: Target,
  version = process.version,
): string {
  if (target.os !== 'win32') {
    throw new Error(
      `native-build: node.lib target is invalid. Where: ${target.triple}. Saw: ${target.os}. Wanted: a Windows target. Fix: request node.lib only for Windows addons.`,
    )
  }
  return `https://nodejs.org/dist/${version}/win-${target.cpu}/node.lib`
}

export function createNativeBuildPlan(
  target: Target,
  options: {
    root?: string | undefined
    toolchain?: string | undefined
    cargoRegistryPath?: string | undefined
  } = {},
): NativeBuildPlan {
  const root = options.root ?? rootPath
  const toolchain = options.toolchain ?? readRustToolchain(root)
  const cargoRegistryPath =
    options.cargoRegistryPath ?? path.join(os.homedir(), '.cargo', 'registry')
  if (target.libc === 'musl') {
    const buildCommand = [
      'apk add --no-cache build-base cmake perl linux-headers',
      'rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal',
      'rustup target add --toolchain "$RUST_TOOLCHAIN" "$RUST_TARGET"',
      'exec rustup run "$RUST_TOOLCHAIN" cargo build --locked --release --target "$RUST_TARGET" -p decmpfs-node',
    ].join('; ')
    const linkerEnv =
      target.cpu === 'arm64'
        ? 'CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=gcc'
        : 'CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=gcc'
    return {
      commands: [
        {
          command: 'docker',
          args: [
            'run',
            '--rm',
            '--mount',
            `type=bind,source=${root},target=/workspace`,
            '--workdir',
            '/workspace',
            '--mount',
            `type=bind,source=${cargoRegistryPath},target=/usr/local/cargo/registry`,
            '--env',
            `RUST_TARGET=${target.rust}`,
            '--env',
            `RUST_TOOLCHAIN=${toolchain}`,
            '--env',
            'CARGO_TERM_COLOR=always',
            '--env',
            'RUST_BACKTRACE=1',
            '--env',
            linkerEnv,
            RUST_IMAGE,
            'sh',
            '-ec',
            buildCommand,
          ],
          cwd: root,
        },
      ],
      artifactPath: path.join(
        root,
        'target',
        target.rust,
        'release',
        target.artifact,
      ),
      alpineLoadCheck: true,
      nativeLoadCheck: false,
    }
  }

  return {
    commands: [
      {
        command: 'rustup',
        args: ['toolchain', 'install', toolchain, '--profile', 'minimal'],
        cwd: root,
      },
      {
        command: 'rustup',
        args: ['target', 'add', '--toolchain', toolchain, target.rust],
        cwd: root,
      },
      {
        command: process.execPath,
        args: [
          path.join(root, 'napi', 'decmpfs', 'scripts', 'build.mts'),
          '--prod',
          '--target',
          target.rust,
        ],
        cwd: path.join(root, 'napi', 'decmpfs'),
        env: { RUSTUP_TOOLCHAIN: toolchain },
      },
    ],
    artifactPath: path.join(root, 'napi', 'decmpfs', 'decmpfs.node'),
    alpineLoadCheck: false,
    nativeLoadCheck: true,
  }
}

function runCommand(command: NativeBuildCommand): void {
  const result = spawnSync(command.command, command.args, {
    cwd: command.cwd,
    env: { ...process.env, ...command.env },
    encoding: 'utf8',
    stdioString: true,
    stdio: ['inherit', 'pipe', 'pipe'],
    maxBuffer: 64 * 1024 * 1024,
  })
  if (result.stdout) {
    logger.log(result.stdout.trimEnd())
  }
  if (result.stderr) {
    logger.log(result.stderr.trimEnd())
  }
  if (result.error) {
    const code =
      'code' in result.error && typeof result.error.code === 'string'
        ? result.error.code
        : result.error.name
    throw new Error(
      `native-build: command failed to start. Where: ${command.command}. Saw: ${code}. Wanted: a successful native build command. Fix: install the required build tool and retry.`,
    )
  }
  if (result.status !== 0) {
    throw new Error(
      `native-build: command failed. Where: ${command.command}. Saw: exit ${result.status ?? result.signal ?? 'unknown'}. Wanted: exit 0. Fix: inspect the build output and retry.`,
    )
  }
}

async function downloadNodeLibrary(target: Target): Promise<string> {
  const directory = path.join(REPO_CACHE_DIR, 'node-lib')
  const destination = path.join(directory, 'node.lib')
  const url = nodeLibraryUrl(target)
  let response: Uint8Array
  try {
    response = await httpBytes(url)
  } catch (cause) {
    const nestedCause =
      typeof cause === 'object' && cause !== null
        ? Reflect.get(cause, 'cause')
        : undefined
    const code =
      typeof nestedCause === 'object' && nestedCause !== null
        ? Reflect.get(nestedCause, 'code')
        : undefined
    const errorType = cause instanceof Error ? cause.name : typeof cause
    throw new Error(
      `native-build: node.lib download failed. Where: ${url}. Saw: ${typeof code === 'string' ? code : errorType}. Wanted: the Windows Node import library. Fix: retry the build when nodejs.org is available.`,
    )
  }
  mkdirSync(directory, { recursive: true })
  writeFileSync(destination, response)
  return directory
}

async function main(): Promise<{
  exitCode: number
  data: { target: string; artifact: string }
}> {
  const json = process.argv.includes('--json')
  const args = process.argv.slice(2).filter(argument => argument !== '--json')
  if (args.length !== 2 || !args[0] || !args[1]) {
    throw new Error(
      `native-build: arguments are invalid. Where: scripts/repo/native/build.mts. Saw: ${args.length} positional arguments. Wanted: <target> <output-dir>. Fix: provide one configured target and one staging directory.`,
    )
  }

  const target = resolveNativeTarget(args[0] ?? '')
  const outputDirectory = path.resolve(rootPath, args[1] ?? '')
  const toolchain = readRustToolchain()
  const plan = createNativeBuildPlan(target, {
    root: rootPath,
    toolchain,
  })
  if (plan.alpineLoadCheck) {
    const cargoRegistryPath = path.join(os.homedir(), '.cargo', 'registry')
    mkdirSync(cargoRegistryPath, { recursive: true })
  }
  const environment: Record<string, string> = {}
  if (target.os === 'win32') {
    environment.NODE_LIB_DIR = await downloadNodeLibrary(target)
  }

  for (const command of plan.commands) {
    runCommand(
      environment.NODE_LIB_DIR
        ? { ...command, env: { ...command.env, ...environment } }
        : command,
    )
  }

  mkdirSync(outputDirectory, { recursive: true })
  const stagedAddonPath = path.join(outputDirectory, 'decmpfs.node')
  copyFileSync(plan.artifactPath, stagedAddonPath)

  if (plan.alpineLoadCheck) {
    runCommand({
      command: 'docker',
      args: [
        'run',
        '--rm',
        '--mount',
        `type=bind,source=${stagedAddonPath},target=/tmp/decmpfs.node,readonly`,
        NODE_IMAGE,
        'node',
        '-e',
        'require("/tmp/decmpfs.node")',
      ],
      cwd: rootPath,
    })
  }

  if (plan.nativeLoadCheck) {
    runCommand({
      command: process.execPath,
      args: ['-e', 'require(process.argv[1])', stagedAddonPath],
      cwd: outputDirectory,
    })
  }

  const result = {
    target: target.triple,
    artifact: stagedAddonPath,
  }
  return { exitCode: 0, data: result }
}

if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
