/**
 * @file Npm release overlay for decmpfs. The canonical fleet bump owns npm
 *   manifests and changelog generation; this wrapper keeps the published Rust
 *   crate and both Cargo locks on that same version before the signed release
 *   commit is created by the protected pipeline.
 */

import { readFileSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

import { spawn } from '@socketsecurity/lib-stable/process/spawn/child'

import { isMainModule } from '../fleet/process/is-main-module.mts'
import { runMain } from '../fleet/process/main/run.mts'
import type { ScriptMeta } from '../fleet/process/main/run.mts'

const rootPath = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
)
const SCRIPT_META: ScriptMeta = {
  describe: 'Bump npm release files and align the decmpfs Rust crate version.',
  help: 'Usage: node scripts/repo/bump.mts [canonical bump options]\nRuns the canonical bump in write-only mode, then syncs Cargo manifests and locks.',
  json: 'native',
}

export function canonicalBumpArgs(argv: readonly string[]): string[] {
  return [...argv.filter(arg => arg !== '--write-only'), '--write-only']
}

function replaceSectionVersion(
  text: string,
  sectionHeader: string,
  sectionName: string,
  version: string,
  filePath: string,
): string {
  const start = text.indexOf(`${sectionHeader}\n`)
  if (start < 0) {
    throw new Error(`bump: ${filePath} has no ${sectionName} section.`)
  }
  const contentStart = start + sectionHeader.length + 1
  const nextHeader = text.slice(contentStart).search(/^\[[^\n]+\]$/mu)
  const end = nextHeader < 0 ? text.length : contentStart + nextHeader
  const section = text.slice(contentStart, end)
  const versionLine = /^version\s*=\s*"[^"]+"/mu
  if (!versionLine.test(section)) {
    throw new Error(`bump: ${filePath} has no version in ${sectionName}.`)
  }
  return (
    text.slice(0, contentStart) +
    section.replace(versionLine, `version = "${version}"`) +
    text.slice(end)
  )
}

function replaceDependencyVersion(text: string, version: string): string {
  const dependency = /^decmpfs\s*=\s*\{[^\n]*\}/mu
  const line = text.match(dependency)?.[0]
  if (!line || !/\bpath\s*=\s*"\.\.\/\.\.\/crates\/decmpfs"/u.test(line)) {
    throw new Error(
      'bump: napi/decmpfs/Cargo.toml has no local decmpfs path dependency.',
    )
  }
  if (!/\bversion\s*=\s*"[^"]+"/u.test(line)) {
    throw new Error(
      'bump: napi/decmpfs/Cargo.toml decmpfs path dependency has no version.',
    )
  }
  return text.replace(
    dependency,
    line.replace(/\bversion\s*=\s*"[^"]+"/u, `version = "${version}"`),
  )
}

function replaceLockPackageVersion(
  text: string,
  version: string,
  filePath: string,
  required: boolean,
): string {
  const starts = [...text.matchAll(/^\[\[package\]\]\s*$/gmu)].map(
    match => match.index!,
  )
  for (let index = 0; index < starts.length; index += 1) {
    const start = starts[index]!
    const end = starts[index + 1] ?? text.length
    const section = text.slice(start, end)
    if (!/^name\s*=\s*"decmpfs"\s*$/mu.test(section)) {
      continue
    }
    const versionLine = /^version\s*=\s*"[^"]+"/mu
    if (!versionLine.test(section)) {
      throw new Error(`bump: ${filePath} decmpfs package entry has no version.`)
    }
    return (
      text.slice(0, start) +
      section.replace(versionLine, `version = "${version}"`) +
      text.slice(end)
    )
  }
  if (required) {
    throw new Error(`bump: ${filePath} has no decmpfs package entry.`)
  }
  return text
}

export function syncCargoVersions(root: string, version: string): void {
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u.test(version)) {
    throw new Error(
      `bump: napi/decmpfs/package.json has an invalid version: ${version}`,
    )
  }
  const crateManifest = path.join(root, 'crates/decmpfs/Cargo.toml')
  const napiManifest = path.join(root, 'napi/decmpfs/Cargo.toml')
  const updates = new Map<string, string>([
    [
      crateManifest,
      replaceSectionVersion(
        readFileSync(crateManifest, 'utf8'),
        '[package]',
        '[package]',
        version,
        crateManifest,
      ),
    ],
    [
      napiManifest,
      replaceDependencyVersion(readFileSync(napiManifest, 'utf8'), version),
    ],
  ])
  for (const relativePath of ['Cargo.lock', 'fuzz/Cargo.lock']) {
    const lockPath = path.join(root, relativePath)
    try {
      const lock = readFileSync(lockPath, 'utf8')
      updates.set(
        lockPath,
        replaceLockPackageVersion(
          lock,
          version,
          relativePath,
          relativePath === 'Cargo.lock',
        ),
      )
    } catch (error) {
      if (
        relativePath === 'fuzz/Cargo.lock' &&
        (error as NodeJS.ErrnoException).code === 'ENOENT'
      ) {
        continue
      }
      throw error
    }
  }
  for (const [filePath, text] of updates) {
    writeFileSync(filePath, text)
  }
}

export function syncResolvedNapiVersion(root: string): void {
  const packageJson = JSON.parse(
    readFileSync(path.join(root, 'napi/decmpfs/package.json'), 'utf8'),
  ) as { version?: unknown }
  if (typeof packageJson.version !== 'string') {
    throw new Error('bump: napi/decmpfs/package.json has no version string.')
  }
  syncCargoVersions(root, packageJson.version)
}

export async function main(): Promise<void> {
  const args = canonicalBumpArgs(process.argv.slice(2))
  const canonical = path.join(rootPath, 'scripts/fleet/release/bump/run.mts')
  const json = args.includes('--json')
  const spawnOptions = json
    ? { cwd: rootPath, stdioString: true as const }
    : { cwd: rootPath, stdio: 'inherit' as const }
  const result = await spawn(
    process.execPath,
    [canonical, ...args],
    spawnOptions,
  )
  if (result.code !== 0) {
    if (json) {
      process.stderr.write(String(result.stderr ?? ''))
      process.stdout.write(String(result.stdout ?? ''))
      process.exitCode = result.code ?? 1
      return
    }
    throw new Error(
      `bump: canonical fleet bump exited ${result.code ?? 'on a signal'}.`,
    )
  }
  if (args.includes('--dry-run')) {
    if (json) {
      process.stderr.write(String(result.stderr ?? ''))
      process.stdout.write(String(result.stdout ?? ''))
    }
    return
  }
  syncResolvedNapiVersion(rootPath)
  if (json) {
    process.stderr.write(String(result.stderr ?? ''))
    process.stdout.write(String(result.stdout ?? ''))
  }
}

if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
