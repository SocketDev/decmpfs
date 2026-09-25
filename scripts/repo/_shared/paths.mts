import path from 'node:path'
import { REPO_ROOT } from '../../fleet/paths.mts'

export * from '../../fleet/paths.mts'
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
