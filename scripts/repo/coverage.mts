// Production-coverage runner. The crate marks its test modules
// `#[cfg_attr(coverage_nightly, coverage(off))]` so the report reflects PRODUCTION
// code, not test fixtures/harness. That attribute needs the unstable
// `coverage_attribute` feature — only NIGHTLY rustc accepts it, and only under a
// nightly toolchain does cargo-llvm-cov set the `coverage_nightly` cfg. On stable
// the markers are inert (tests would inflate the number) and the feature gate
// errors, so this script REQUIRES nightly + the llvm-tools component and fails
// loud (What / Where / Saw-vs-wanted / Fix) rather than reporting a wrong number.
//
//   node scripts/repo/coverage.mts                 # full annotated report
//   node scripts/repo/coverage.mts --summary-only  # table only
//   node scripts/repo/coverage.mts --json --summary-only

import { existsSync, readdirSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
import { getDefaultLogger } from '@socketsecurity/lib-stable/logger/default'
import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

const logger = getDefaultLogger()

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..')

function fail(what: string, fix: string): never {
  logger.fail(`coverage: ${what}`)
  logger.fail(`  fix: ${fix}`)
  process.exit(1)
}

// Resolve a nightly toolchain via rustup: prefer the rolling `nightly` channel,
// else the newest dated nightly.
function resolveNightly(): string {
  const listResult = spawnSync('rustup', ['toolchain', 'list'], {
    encoding: 'utf8',
  })
  if (listResult.status !== 0) {
    fail(
      'rustup not found — coverage needs a rustup-managed nightly toolchain',
      'install rustup (https://rustup.rs), then `rustup toolchain install nightly`',
    )
  }
  const list = listResult.stdout
  const names = list
    .split('\n')
    .map(line => line.trim().replace(/ \(.*\)$/, ''))
    .filter(Boolean)
  const rolling = names.find(
    name => name.startsWith('nightly-') && !/^nightly-\d/.test(name),
  )
  const dated = names
    .filter(name => /^nightly-\d{4}-\d\d-\d\d/.test(name))
    .toSorted()
    .at(-1)
  const toolchain = rolling ?? dated
  if (!toolchain) {
    fail(
      'no nightly toolchain installed — the coverage(off) markers need nightly rustc',
      'rustup toolchain install nightly && rustup component add llvm-tools-preview --toolchain nightly',
    )
  }
  return toolchain
}

const toolchain = resolveNightly()
const tcRoot = path.join(os.homedir(), '.rustup', 'toolchains', toolchain)
const rustc = path.join(tcRoot, 'bin', 'rustc')
const cargo = path.join(tcRoot, 'bin', 'cargo')
if (!existsSync(rustc) || !existsSync(cargo)) {
  fail(
    `nightly toolchain ${toolchain} is missing rustc/cargo under ${tcRoot}`,
    `rustup toolchain install ${toolchain}`,
  )
}

// The feature gate rejects a stable channel — confirm we really have nightly.
const version = String(
  spawnSync(rustc, ['--version'], { encoding: 'utf8' }).stdout ?? '',
).trim()
if (!version.includes('nightly')) {
  fail(
    `resolved rustc is not nightly (${version}) — coverage_attribute is nightly-only`,
    'rustup toolchain install nightly',
  )
}

// Locate the toolchain's own llvm-cov / llvm-profdata (the llvm-tools component);
// the socket shim on PATH would otherwise resolve a stable rustc/llvm.
function llvmBin(name: string): string {
  const base = path.join(tcRoot, 'lib', 'rustlib')
  const triples = existsSync(base) ? readdirSync(base) : []
  for (const triple of triples) {
    const candidate = path.join(base, triple, 'bin', name)
    if (existsSync(candidate)) {
      return candidate
    }
  }
  return ''
}
const llvmCov = llvmBin('llvm-cov')
const llvmProfdata = llvmBin('llvm-profdata')
if (!llvmCov || !llvmProfdata) {
  fail(
    `llvm-tools missing from ${toolchain} (no llvm-cov / llvm-profdata)`,
    `rustup component add llvm-tools-preview --toolchain ${toolchain}`,
  )
}

logger.log(`coverage: nightly=${toolchain}`)
const covResult = spawnSync(
  cargo,
  [
    'llvm-cov',
    '--lib',
    '-p',
    'decmpfs',
    ...process.argv.slice(2),
    // Run the ignored perf probes too, so their bodies count and the number is
    // production coverage, not probe-body-deflated.
    '--',
    '--include-ignored',
  ],
  {
    cwd: root,
    stdio: 'inherit',
    env: {
      ...process.env,
      LLVM_COV: llvmCov,
      LLVM_PROFDATA: llvmProfdata,
      RUSTC: rustc,
      RUSTUP_TOOLCHAIN: toolchain,
    },
  },
)
if (covResult.status !== 0) {
  process.exit(1)
}
