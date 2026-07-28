// Pure, side-effect-free helpers for the decmpfs release entry (release.mts).
// Kept importable + no-I/O so they can be unit-tested with no network and no
// filesystem — the release script wires this to real files, git, and cargo.

export interface Resolution {
  version: string
  mode: 'finalize' | 'bump' | 'as-committed'
}

// Resolve the version to release from the committed version and an optional arg:
//   - a `-prerelease` (or any `-suffix`) committed version + no arg → FINALIZE
//     to the plain semver (0.1.1-prerelease → 0.1.1), reusing the CHANGELOG
//     section already written for it, kept verbatim and never re-stubbed;
//   - a new semver arg → BUMP (insert a CHANGELOG stub to fill in);
//   - otherwise release WHAT IS COMMITTED.
export function resolveRelease(
  current: string,
  argVersion: string,
): Resolution {
  const pre = current.match(/^(?<base>\d+\.\d+\.\d+)-[0-9A-Za-z.-]+$/)
  if (pre && !argVersion) {
    return { version: pre.groups!['base']!, mode: 'finalize' }
  }
  if (argVersion && argVersion !== current) {
    return { version: argVersion, mode: 'bump' }
  }
  return { version: current, mode: 'as-committed' }
}

// Conventional-commit types that produce user-facing changelog bullets, in
// section order, with their section titles. The shape mirrors the
// conventional-changelog config-spec `types` array (type/section/hidden), so
// the output matches what the conventionalcommits preset would render — the
// spec vocabulary without its dependency tree.
const CHANGELOG_TYPES = [
  { section: 'Features', type: 'feat' },
  { section: 'Bug Fixes', type: 'fix' },
  { section: 'Performance', type: 'perf' },
] as const

// Scopes whose commits are repo plumbing (CI wiring, dep churn, fleet
// cascades, lint waves, test scaffolding) — dropped even when the type is
// feat/fix/perf, because they change how the repo is built, not what a
// consumer gets.
const PLUMBING_SCOPES = new Set([
  'cascade',
  'ci',
  'coverage',
  'deps',
  'docs',
  'fleet',
  'lint',
  'release',
  'repo',
  'test',
  'tests',
  'wheelhouse',
])

// Fleet-churn subjects that slip through scope parsing (catalog pins, mirror
// refreshes, lint waves) — plumbing regardless of their type token.
const PLUMBING_SUBJECT_RE =
  /catalog pin|catalog to|lint --fix|mirrors from bundle|cascade template/i

// Parse one conventional-commit subject: `type(scope)!: description`.
const SUBJECT_RE = /^(?<type>[a-z]+)(?:\((?<scope>[^)]*)\))?!?:\s*(?<desc>.+)$/

// Derive the user-facing changelog groups for a release from the commit
// subjects since the previous tag: `{ section, bullets }` per visible type,
// empty sections dropped, exact duplicate subjects collapsed, scoped
// subjects keeping their scope as a `**scope:**` lead-in (the
// conventionalcommits preset's rendering).
export function changelogEntries(
  subjects: readonly string[],
): Array<{ bullets: string[]; section: string }> {
  const byType = new Map<string, string[]>()
  for (const { type } of CHANGELOG_TYPES) {
    byType.set(type, [])
  }
  const seen = new Set<string>()
  for (const subject of subjects) {
    const match = subject.match(SUBJECT_RE)
    if (!match?.groups) {
      continue
    }
    const type = match.groups['type']!
    const scope = match.groups['scope']
    const desc = match.groups['desc']!
    if (!byType.has(type)) {
      continue
    }
    if (scope && PLUMBING_SCOPES.has(scope)) {
      continue
    }
    if (PLUMBING_SUBJECT_RE.test(subject)) {
      continue
    }
    const key = `${type}:${scope ?? ''}:${desc}`
    if (seen.has(key)) {
      continue
    }
    seen.add(key)
    const sentence = desc + (/[.!?]$/.test(desc) ? '' : '')
    byType
      .get(type)!
      .push(scope ? `- **${scope}:** ${sentence}` : `- ${sentence}`)
  }
  const groups: Array<{ bullets: string[]; section: string }> = []
  for (const { section, type } of CHANGELOG_TYPES) {
    const bullets = byType.get(type)!
    if (bullets.length > 0) {
      groups.push({ bullets, section })
    }
  }
  return groups
}

// The full `## <version>` CHANGELOG section for a bump, derived from the
// commit subjects since the previous tag and rendered in the
// conventionalcommits shape (`### Features` / `### Bug Fixes` /
// `### Performance`). When nothing user-facing survives the plumbing
// filter, the section says so plainly instead of stubbing a TODO for a
// human to fill.
export function changelogSection(
  version: string,
  subjects: readonly string[],
): string {
  const groups = changelogEntries(subjects)
  if (groups.length === 0) {
    return `## ${version}\n\n- Maintenance release; no user-facing changes.\n`
  }
  const parts: string[] = [`## ${version}`]
  for (const { bullets, section } of groups) {
    parts.push('', `### ${section}`, '', ...bullets)
  }
  return parts.join('\n') + '\n'
}
