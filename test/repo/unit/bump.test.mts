import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import os from 'node:os'
import path from 'node:path'

import { describe, expect, test } from 'vitest'

import {
  canonicalBumpArgs,
  syncResolvedNapiVersion,
} from '../../../scripts/repo/bump.mts'

describe('decmpfs npm bump overlay', () => {
  test('forces canonical write-only mode while preserving release options', () => {
    expect(canonicalBumpArgs(['--release-as', '0.1.6', '--dry-run'])).toEqual([
      '--release-as',
      '0.1.6',
      '--dry-run',
      '--write-only',
    ])
    expect(
      canonicalBumpArgs(['--write-only', '--release-as', '0.1.6']),
    ).toEqual(['--release-as', '0.1.6', '--write-only'])
  })

  test('syncs the resolved npm version into exact Cargo package entries', () => {
    const root = mkdtempSync(path.join(os.tmpdir(), 'decmpfs-bump-'))
    try {
      mkdirSync(path.join(root, 'napi/decmpfs'), { recursive: true })
      mkdirSync(path.join(root, 'crates/decmpfs'), { recursive: true })
      mkdirSync(path.join(root, 'fuzz'), { recursive: true })
      writeFileSync(
        path.join(root, 'napi/decmpfs/package.json'),
        '{"version":"0.1.6"}',
      )
      writeFileSync(
        path.join(root, 'crates/decmpfs/Cargo.toml'),
        '[package]\nname = "decmpfs"\nversion = "0.1.5"\n\n[dependencies]\nzstd = { version = "=0.13.3" }\n',
      )
      writeFileSync(
        path.join(root, 'napi/decmpfs/Cargo.toml'),
        '[package]\nname = "decmpfs-node"\nversion = "0.0.0"\n\n[dependencies]\ndecmpfs = { path = "../../crates/decmpfs", version = "0.1.4", features = ["exe"] }\nother = { version = "8.2.1" }\n',
      )
      const lock =
        '[package]\nname = "ignored"\nversion = "0.1.5"\n\n[[package]]\nname = "decmpfs"\nversion = "0.1.5"\ndependencies = ["zstd"]\n\n[[package]]\nname = "decmpfs-helper"\nversion = "2.0.0"\n'
      writeFileSync(path.join(root, 'Cargo.lock'), lock)
      writeFileSync(
        path.join(root, 'fuzz/Cargo.lock'),
        '[[package]]\nname = "decmpfs"\nversion = "0.1.1-prerelease"\n\n[[package]]\nname = "decmpfs-fuzz"\nversion = "0.0.0"\n',
      )

      syncResolvedNapiVersion(root)

      expect(
        readFileSync(path.join(root, 'crates/decmpfs/Cargo.toml'), 'utf8'),
      ).toContain('version = "0.1.6"')
      expect(
        readFileSync(path.join(root, 'crates/decmpfs/Cargo.toml'), 'utf8'),
      ).toContain('zstd = { version = "=0.13.3" }')
      expect(
        readFileSync(path.join(root, 'napi/decmpfs/Cargo.toml'), 'utf8'),
      ).toContain(
        'decmpfs = { path = "../../crates/decmpfs", version = "0.1.6", features = ["exe"] }',
      )
      expect(
        readFileSync(path.join(root, 'napi/decmpfs/Cargo.toml'), 'utf8'),
      ).toContain('other = { version = "8.2.1" }')
      expect(readFileSync(path.join(root, 'Cargo.lock'), 'utf8')).toBe(
        lock.replace(
          'version = "0.1.5"\ndependencies = ["zstd"]',
          'version = "0.1.6"\ndependencies = ["zstd"]',
        ),
      )
      expect(
        readFileSync(path.join(root, 'fuzz/Cargo.lock'), 'utf8'),
      ).toContain('name = "decmpfs"\nversion = "0.1.6"')
      expect(
        readFileSync(path.join(root, 'fuzz/Cargo.lock'), 'utf8'),
      ).toContain('name = "decmpfs-fuzz"\nversion = "0.0.0"')
    } finally {
      rmSync(root, { force: true, recursive: true })
    }
  })
})
