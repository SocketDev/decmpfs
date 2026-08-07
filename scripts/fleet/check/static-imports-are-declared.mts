#!/usr/bin/env node
/**
 * @file `check --all` gate: every STATIC bare-specifier import in a cascaded
 *   `.mts` — a fleet or repo hook (`.claude/hooks/{fleet,repo}/**\/*.mts`) or a
 *   fleet or repo script (`scripts/{fleet,repo}/**\/*.mts`) — must be declared
 *   in the repo root `package.json`'s `dependencies` or `devDependencies`. A
 *   cascaded file that imports a package the manifest doesn't declare inherits
 *   a broken import at runtime for every member that installs from the manifest
 *   but never gets the transitive package on disk — the incident this check
 *   exists to catch: `check-new-deps` imported
 *   `@socketregistry/packageurl-js-stable` and `@socketsecurity/sdk-stable`
 *   while the wheelhouse root `package.json` declared neither, so every member
 *   inherited a hook whose imports weren't installed.
 *   The script trees are covered for the same reason and by the same mechanism.
 *   Second incident, the one that widened this check past hooks:
 *   `scripts/fleet/gen/hook-validators.mts` imports `Code` from
 *   `typebox/compile` as a VALUE, and bun-security-scanner declared no
 *   `typebox` — so four checks died with `ERR_MODULE_NOT_FOUND` the moment its
 *   CI got far enough to run them. Hook-only scanning could not see it.
 *   Scans every `.mts` file under each tree in SCANNED_TREES; a tree that is
 *   absent (not every member carries `.claude/hooks/repo/` or
 *   `scripts/repo/`) is a no-op, never an error. Extracts every STATIC
 *   bare-specifier import: `import … from '<spec>'`, the bare side-effect form
 *   `import '<spec>'`, and `export … from '<spec>'` re-exports. Dynamic
 *   `import(...)` and `require(...)` are not static and are never matched. A
 *   `node:*` builtin or a relative (`./`, `../`) specifier names no installed
 *   package and is skipped. A subpath import (e.g.
 *   `@socketsecurity/lib-stable/logger/default`) resolves to its package name
 *   (`@socketsecurity/lib-stable`) before the declared-deps check.
 *   A TYPE-ONLY import (`import type … from '<spec>'` / `export type … from`)
 *   is additionally satisfied by the DefinitelyTyped package for that name,
 *   because that is what actually resolves it: `scripts/fleet/lib/
 *   markdown-ast.mts` does `import type { Root } from 'mdast'`, and `mdast` is
 *   not a package anyone installs — `@types/mdast` supplies those types and IS
 *   declared. Without this rule the widened check reports `mdast` on every repo
 *   in the fleet and cries wolf. The fallback is deliberately NOT extended to
 *   value imports: `@types/parse5` does not put `parseFragment` on disk, so
 *   `import { parseFragment } from 'parse5'` still demands real `parse5`.
 *   A second pass covers the tier the root manifest cannot see: a hook dir
 *   holding its own `package.json` is a pnpm workspace package, so pnpm links
 *   into it only what THAT manifest declares. An import the hook manifest omits
 *   resolves by hoisting luck alone — green until the next prune/relink, then
 *   every hook event dies at once with `ERR_MODULE_NOT_FOUND: Cannot find
 *   package '@socketsecurity/lib-stable'`. Hook dirs without a manifest
 *   (`_shared`, `_dist`, `_shared`) resolve against the root and are covered
 *   by the first pass alone.
 *   Fails loud (What / Where / Saw / Wanted / Fix) listing every undeclared
 *   specifier + the importing file — never a silent skip. Usage: node
 *   scripts/fleet/check/static-imports-are-declared.mts [--quiet]
 */

import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'

import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'

import { PACKAGE_JSON, REPO_ROOT } from '../paths.mts'
import { isMainModule } from '../_shared/is-main-module.mts'
import { runMain } from '../_shared/run-main.mts'

import type { ScriptMeta } from '../_shared/run-main.mts'

const logger = getDefaultLogger()

// Cascaded `.mts` trees to scan, relative to REPO_ROOT. The `repo` halves are
// repo-specific and the `template/base` halves are wheelhouse-only, so most
// members carry a subset; listMtsFiles returns `[]` for a missing directory, so
// an absent tree is simply a no-op — never an error.
export const SCANNED_TREES: readonly string[] = [
  path.join('.claude', 'hooks', 'fleet'),
  path.join('.claude', 'hooks', 'repo'),
  path.join('scripts', 'fleet'),
  path.join('scripts', 'repo'),
  path.join('template', 'base', '.claude', 'hooks', 'fleet'),
  path.join('template', 'base', '.claude', 'hooks', 'repo'),
  path.join('template', 'base', 'scripts', 'fleet'),
  path.join('template', 'base', 'scripts', 'repo'),
]

export interface ImportRef {
  readonly specifier: string
  readonly typeOnly: boolean
}

export interface UndeclaredImport {
  readonly file: string
  readonly packageName: string
  readonly specifier: string
}

export interface UndeclaredWorkspaceImport {
  readonly file: string
  readonly manifest: string
  readonly packageName: string
  readonly specifier: string
}

interface PackageJsonShape {
  dependencies?: Record<string, string> | undefined
  devDependencies?: Record<string, string> | undefined
}

/**
 * Recursively list every `.mts` file under `dir`. Returns `[]` for a missing
 * or unreadable directory — a repo with no `.claude/hooks/repo/` tree is not
 * an error, just nothing to scan.
 */
export function listMtsFiles(dir: string): string[] {
  let entries: string[]
  try {
    entries = readdirSync(dir)
  } catch {
    return []
  }
  const out: string[] = []
  for (let i = 0, { length } = entries; i < length; i += 1) {
    const name = entries[i]!
    if (name === 'node_modules' || name.startsWith('.')) {
      continue
    }
    const full = path.join(dir, name)
    let isDir: boolean
    try {
      isDir = statSync(full).isDirectory()
    } catch {
      continue
    }
    if (isDir) {
      out.push(...listMtsFiles(full))
    } else if (name.endsWith('.mts')) {
      out.push(full)
    }
  }
  return out
}

/**
 * Extract every STATIC bare-specifier import's raw specifier string out of a
 * `.mts` file's content: `import … from '<spec>'` / `export … from '<spec>'`
 * (both including a `type` keyword), and the bare side-effect form
 * `import '<spec>'`. Dynamic `import(...)`/`require(...)` are never matched.
 *
 * The gap between the `import`/`export` keyword and `from` is deliberately
 * restricted to `[\s\w,{}*]` — whitespace, identifiers, commas, braces, `*` —
 * not a naive `[^;]*?`. TS/JS statements don't require a terminating `;`
 * (ASI), so an unbounded gap can walk through an unrelated `export const x =
 * [...]` all the way to an UNRELATED later `from` inside a string literal
 * (e.g. a hook's own `lines.push('... cascade from ...')` text), capturing
 * garbage as the "specifier". The restricted class can't cross `=`, `(`,
 * quotes, or other real-statement punctuation, so a false start fails
 * outright instead of running away — while still admitting the shapes the
 * fleet's `.mts` imports actually use: named/default/namespace imports,
 * `type`/`as` keywords, and `export … from` re-exports.
 */
export function extractImportSpecifiers(content: string): string[] {
  return extractImportRefs(content).map(ref => ref.specifier)
}

/**
 * The same extraction as `extractImportSpecifiers`, keeping the one bit that
 * decides which declaration satisfies each import: whether the clause was
 * TYPE-ONLY (`import type … from` / `export type … from`). A type-only import
 * is erased before runtime, so a DefinitelyTyped package resolves it; a value
 * import is not, so only the real package does. See `isImportSatisfied`.
 *
 * Only the leading `type` KEYWORD counts. An inline modifier (`import { type
 * Foo, bar } from 'x'`) is deliberately read as a value import: the clause can
 * mix type and value bindings, and treating it as type-only would let a
 * `@types/` entry satisfy a real runtime dependency. Reading it as a value
 * import can only over-report, never mask.
 */
export function extractImportRefs(content: string): ImportRef[] {
  const refs: ImportRef[] = []
  const fromRe =
    // Captures the clause between the keyword and `from` (group 1, inspected
    // for a leading `type`) and the module specifier (group 2).
    // oxlint-disable-next-line socket/require-regex-comment -- captures
    /(?:^|\n)[ \t]*(?:export|import)\b([\s\w,{}*]*?)\bfrom[ \t]*['"]([^'"]+)['"]/g
  let m: RegExpExecArray | null
  while ((m = fromRe.exec(content)) !== null) {
    refs.push({ specifier: m[2]!, typeOnly: isTypeOnlyClause(m[1]!) })
  }
  // Bare side-effect import: `import '<spec>'` (no `from`). Anchored so the
  // first non-space token after `import` must be a quote, so it never
  // re-matches a `from`-form line already caught above. Never type-only —
  // a side-effect import exists precisely to run the module.
  // oxlint-disable-next-line socket/require-regex-comment -- described above
  const sideEffectRe = /(?:^|\n)[ \t]*import[ \t]*['"]([^'"]+)['"]/g
  while ((m = sideEffectRe.exec(content)) !== null) {
    refs.push({ specifier: m[1]!, typeOnly: false })
  }
  return refs
}

/**
 * Whether the clause between `import`/`export` and `from` opens with the `type`
 * KEYWORD. A clause that is exactly `type` is a DEFAULT BINDING NAMED `type`
 * (`import type from 'x'`), not a type-only import, so it reads as a value
 * import — the conservative direction.
 */
export function isTypeOnlyClause(clause: string): boolean {
  return /^\s+type\b/.test(clause) && clause.trim() !== 'type'
}

/**
 * The DefinitelyTyped package that supplies types for `packageName`, under the
 * standard `@types/` mangling: `mdast` → `@types/mdast`, and a scoped name
 * flattens its slash to a double underscore, `@scope/name` →
 * `@types/scope__name`.
 */
export function typesPackageName(packageName: string): string {
  if (packageName.startsWith('@')) {
    return `@types/${packageName.slice(1).replace('/', '__')}`
  }
  return `@types/${packageName}`
}

/**
 * Whether `declaredNames` satisfies `ref`, an import that resolved to
 * `packageName`. The package's own name always satisfies it. A TYPE-ONLY import
 * is additionally satisfied by its `@types/` package, which is the thing that
 * actually resolves the types when the runtime package does not exist
 * (`mdast`, `estree`). A value import is never satisfied by `@types/` alone —
 * those ship declarations, not code.
 */
export function isImportSatisfied(
  ref: ImportRef,
  packageName: string,
  declaredNames: ReadonlySet<string>,
): boolean {
  return (
    declaredNames.has(packageName) ||
    (ref.typeOnly && declaredNames.has(typesPackageName(packageName)))
  )
}

/**
 * Resolve a bare import specifier to the package name that must be declared
 * in `package.json`: `@scope/name` for a scoped package, subpath dropped, or
 * the first path segment for an unscoped package. Returns `undefined` for a
 * relative (`.`/`..`) or `node:` builtin specifier — neither names an
 * installed package.
 */
export function packageNameFromSpecifier(
  specifier: string,
): string | undefined {
  if (
    specifier === '' ||
    specifier.startsWith('.') ||
    specifier.startsWith('node:')
  ) {
    return undefined
  }
  if (specifier.startsWith('@')) {
    const [scope, name] = specifier.split('/')
    return scope && name ? `${scope}/${name}` : undefined
  }
  return specifier.split('/')[0]
}

/**
 * Read `dependencies` + `devDependencies` keys off `packageJsonPath` into one
 * declared-names set. A missing/unparseable `package.json` yields an empty
 * set — fail loud downstream, every import reads as undeclared, which
 * correctly signals the manifest itself is broken rather than silently
 * passing.
 */
export function readDeclaredPackageNames(packageJsonPath: string): Set<string> {
  let pkg: PackageJsonShape
  try {
    pkg = JSON.parse(readFileSync(packageJsonPath, 'utf8')) as PackageJsonShape
  } catch {
    return new Set()
  }
  return new Set([
    ...Object.keys(pkg.dependencies ?? {}),
    ...Object.keys(pkg.devDependencies ?? {}),
  ])
}

/**
 * Diagnose every hook file in `files` (relative path → content) for a
 * bare-specifier import whose resolved package name is NOT in
 * `declaredNames`. Pure — the check's whole finding logic, independent of
 * file-system layout, so unit tests can drive it with in-memory fixtures.
 * Deduplicates by (file, packageName) so a package imported via several
 * subpaths in one file is reported once.
 */
export function findUndeclaredImports(
  files: ReadonlyMap<string, string>,
  declaredNames: ReadonlySet<string>,
): UndeclaredImport[] {
  const findings: UndeclaredImport[] = []
  const seen = new Set<string>()
  for (const [file, content] of files) {
    for (const ref of extractImportRefs(content)) {
      const { specifier } = ref
      const packageName = packageNameFromSpecifier(specifier)
      if (
        packageName === undefined ||
        isImportSatisfied(ref, packageName, declaredNames)
      ) {
        continue
      }
      const key = `${file} ${packageName}`
      if (seen.has(key)) {
        continue
      }
      seen.add(key)
      findings.push({ file, packageName, specifier })
    }
  }
  findings.sort(
    (a, b) =>
      a.file.localeCompare(b.file) ||
      a.packageName.localeCompare(b.packageName),
  )
  return findings
}

/**
 * The manifest that actually governs `fileAbs`'s bare-specifier resolution:
 * the nearest `package.json` at or above the file's directory, bounded by
 * `repoRoot`. A hook dir carrying its own `package.json` is a pnpm workspace
 * package, so that manifest — NOT the repo root one — decides what gets linked
 * into its `node_modules`. A hook dir without one (`_shared`, `_dist`,
 * `_shared`) resolves against the root manifest.
 */
export function governingManifestPath(
  fileAbs: string,
  repoRoot: string,
): string {
  const rootManifest = path.join(repoRoot, 'package.json')
  let dir = path.dirname(fileAbs)
  while (
    dir !== repoRoot &&
    dir.startsWith(repoRoot) &&
    dir !== path.dirname(dir)
  ) {
    const candidate = path.join(dir, 'package.json')
    if (existsSync(candidate)) {
      return candidate
    }
    dir = path.dirname(dir)
  }
  return rootManifest
}

/**
 * Diagnose every hook file whose GOVERNING manifest (its own hook
 * `package.json`) omits a package it imports. This is the tier the
 * root-manifest pass cannot see: pnpm's isolated `node_modules` links into a
 * workspace package only what that package's own manifest declares, so an
 * import the hook manifest omits resolves by hoisting luck alone — green until
 * the next prune/relink, then every hook event dies at once with
 * `ERR_MODULE_NOT_FOUND`. Files governed by the root manifest are skipped here;
 * `findUndeclaredImports` already covers them. Deduplicated by (manifest,
 * packageName) so one missing entry is reported once, not once per import
 * site.
 */
export function findUndeclaredWorkspaceImports(
  files: ReadonlyMap<string, string>,
  repoRoot: string,
): UndeclaredWorkspaceImport[] {
  const rootManifest = path.join(repoRoot, 'package.json')
  const declaredByManifest = new Map<string, Set<string>>()
  const findings: UndeclaredWorkspaceImport[] = []
  const seen = new Set<string>()
  for (const [file, content] of files) {
    const manifestPath = governingManifestPath(
      path.join(repoRoot, file),
      repoRoot,
    )
    if (manifestPath === rootManifest) {
      continue
    }
    let declared = declaredByManifest.get(manifestPath)
    if (!declared) {
      declared = readDeclaredPackageNames(manifestPath)
      declaredByManifest.set(manifestPath, declared)
    }
    const manifest = path.relative(repoRoot, manifestPath)
    const refs = extractImportRefs(content)
    for (let i = 0, { length } = refs; i < length; i += 1) {
      const ref = refs[i]!
      const { specifier } = ref
      const packageName = packageNameFromSpecifier(specifier)
      if (
        packageName === undefined ||
        isImportSatisfied(ref, packageName, declared)
      ) {
        continue
      }
      const key = `${manifest} ${packageName}`
      if (seen.has(key)) {
        continue
      }
      seen.add(key)
      findings.push({ file, manifest, packageName, specifier })
    }
  }
  return findings.toSorted(function byManifestThenPackage(a, b) {
    return (
      a.manifest.localeCompare(b.manifest) ||
      a.packageName.localeCompare(b.packageName)
    )
  })
}

/**
 * Read every `.mts` file under each of `trees` into a relative-path →
 * content map (relative to `repoRoot`). Unreadable files are skipped, never
 * fatal.
 */
export function readScannedFiles(
  repoRoot: string,
  trees: readonly string[],
): Map<string, string> {
  const files = new Map<string, string>()
  for (let i = 0, { length } = trees; i < length; i += 1) {
    const dir = path.join(repoRoot, trees[i]!)
    const mtsFiles = listMtsFiles(dir)
    for (let j = 0, fl = mtsFiles.length; j < fl; j += 1) {
      const file = mtsFiles[j]!
      let content: string
      try {
        content = readFileSync(file, 'utf8')
      } catch {
        continue
      }
      files.set(path.relative(repoRoot, file), content)
    }
  }
  return files
}

export function main(): void {
  const quiet = process.argv.includes('--quiet')
  const declaredNames = readDeclaredPackageNames(PACKAGE_JSON)
  const files = readScannedFiles(REPO_ROOT, SCANNED_TREES)
  const findings = findUndeclaredImports(files, declaredNames)

  if (findings.length > 0) {
    logger.fail(
      '[static-imports-are-declared] a cascaded file imports a package the root package.json does not declare.',
    )
    logger.error('')
    logger.error(
      '  What:   every bare-specifier import in a .claude/hooks/{fleet,repo} or',
    )
    logger.error(
      '          scripts/{fleet,repo} file must resolve to a name in package.json',
    )
    logger.error(
      '          dependencies/devDependencies — otherwise a member installs the file',
    )
    logger.error('          without the package it imports.')
    logger.error('')
    for (let i = 0, { length } = findings; i < length; i += 1) {
      const f = findings[i]!
      logger.error(`  Where:  ${f.file}`)
      logger.error(
        `  Saw:    import '${f.specifier}' → package "${f.packageName}", not declared`,
      )
      logger.error(
        `  Wanted: "${f.packageName}" in package.json dependencies or devDependencies`,
      )
      logger.error('')
    }
    logger.error(
      '  Fix:    add each missing package to package.json (catalog: if a fleet-canonical',
    )
    logger.error(
      '          catalog entry exists — see scripts/repo/sync-scaffolding/manifest/catalog.mts)',
    )
    logger.error('          and run `pnpm i`.')
    process.exitCode = 1
    return
  }

  const workspaceFindings = findUndeclaredWorkspaceImports(files, REPO_ROOT)
  if (workspaceFindings.length > 0) {
    logger.fail(
      '[static-imports-are-declared] a hook imports a package its OWN package.json does not declare.',
    )
    logger.error('')
    logger.error(
      '  What:   a hook dir holding a package.json is its own pnpm workspace package,',
    )
    logger.error(
      '          so pnpm links into it only what THAT manifest declares. An undeclared',
    )
    logger.error(
      '          import resolves by hoisting luck and dies with ERR_MODULE_NOT_FOUND on',
    )
    logger.error(
      '          the next prune/relink, silencing every hook event at once.',
    )
    logger.error('')
    for (let i = 0, { length } = workspaceFindings; i < length; i += 1) {
      const f = workspaceFindings[i]!
      logger.error(`  Where:  ${f.file}`)
      logger.error(
        `  Saw:    import '${f.specifier}' → package "${f.packageName}", absent from ${f.manifest}`,
      )
      logger.error(
        `  Wanted: "${f.packageName}": "catalog:" in ${f.manifest} dependencies`,
      )
      logger.error('')
    }
    logger.error(
      '  Fix:    add each missing package to the hook manifest under template/base/ first,',
    )
    logger.error(
      '          then cascade (node scripts/repo/sync-scaffolding/cli.mts --target . --fix)',
    )
    logger.error('          and run `pnpm i`.')
    process.exitCode = 1
    return
  }

  if (!quiet) {
    logger.success(
      '[static-imports-are-declared] every static import resolves to a declared package.json dependency.',
    )
  }
}

const SCRIPT_META: ScriptMeta = {
  describe:
    'verifies every static hook import is declared in the owning package.json',
  help: `Usage: node scripts/fleet/check/static-imports-are-declared.mts [flags]

  --quiet  suppress the success message`,
}

if (isMainModule(import.meta.url)) {
  runMain(main, SCRIPT_META)
}
