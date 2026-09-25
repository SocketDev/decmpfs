import path from 'node:path'
import { fileURLToPath } from 'node:url'

export const REPO_ROOT = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  '..',
)
export const CRATE_MANIFEST_PATH = path.join(
  REPO_ROOT,
  'crates',
  'decmpfs',
  'Cargo.toml',
)
export const CHECK_VERSIONS_SCRIPT_PATH = path.join(
  REPO_ROOT,
  'scripts',
  'repo',
  'check-versions.mts',
)
