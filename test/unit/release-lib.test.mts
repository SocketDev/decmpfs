import assert from 'node:assert/strict'
import { test } from 'vitest'

import {
  changelogEntries,
  changelogSection,
  resolveRelease,
} from '../../scripts/repo/release-lib.mts'

test('resolveRelease: prerelease hint finalizes, arg bumps, else as-committed', () => {
  assert.deepEqual(resolveRelease('0.1.1-prerelease', ''), {
    version: '0.1.1',
    mode: 'finalize',
  })
  assert.deepEqual(resolveRelease('0.1.0', '0.2.0'), {
    version: '0.2.0',
    mode: 'bump',
  })
  assert.deepEqual(resolveRelease('0.1.0', ''), {
    version: '0.1.0',
    mode: 'as-committed',
  })
  // An arg equal to the current version is not a bump.
  assert.deepEqual(resolveRelease('0.1.0', '0.1.0'), {
    version: '0.1.0',
    mode: 'as-committed',
  })
  // A build/other suffix also finalizes.
  assert.deepEqual(resolveRelease('2.3.4-rc.1', ''), {
    version: '2.3.4',
    mode: 'finalize',
  })
})

test('changelogEntries: keeps feat/fix/perf sections, drops plumbing', () => {
  const subjects = [
    'feat(node): copyDecmpfsFile gains errorOnExist',
    'fix(remove): allow current_dir at the Io-seam cwd + its guard test',
    'perf(lzvn): parallel block compression across cores',
    'chore(deps): bump @socketsecurity/lib catalog pin to 6.2.2',
    'fix(ci): bootstrap checkout with persist-credentials:false',
    'style: lint --fix --all wave',
    'chore(wheelhouse): cascade template@1f9cbd9ed',
    'fix(napi): resync pnpm-lock with the platform packages',
    'refactor(config): move socket-wheelhouse.json to .config/repo',
  ]
  assert.deepEqual(changelogEntries(subjects), [
    {
      bullets: ['- **node:** copyDecmpfsFile gains errorOnExist'],
      section: 'Features',
    },
    {
      bullets: [
        '- **remove:** allow current_dir at the Io-seam cwd + its guard test',
        '- **napi:** resync pnpm-lock with the platform packages',
      ],
      section: 'Bug Fixes',
    },
    {
      bullets: ['- **lzvn:** parallel block compression across cores'],
      section: 'Performance',
    },
  ])
})

test('changelogEntries: dedupes repeats, skips non-conventional subjects', () => {
  const subjects = [
    'fix(node): surface fs-shaped errors',
    'fix(node): surface fs-shaped errors',
    'merge branch main',
  ]
  assert.deepEqual(changelogEntries(subjects), [
    {
      bullets: ['- **node:** surface fs-shaped errors'],
      section: 'Bug Fixes',
    },
  ])
})

test('changelogSection: renders conventionalcommits-shaped sections', () => {
  assert.equal(
    changelogSection('0.2.0', [
      'feat: pack executables',
      'fix(rm): retry on EBUSY',
    ]),
    '## 0.2.0\n\n### Features\n\n- pack executables\n\n### Bug Fixes\n\n- **rm:** retry on EBUSY\n',
  )
})

test('changelogSection: falls back to a plain maintenance line', () => {
  assert.equal(
    changelogSection('0.1.2', ['chore(deps): bump something']),
    '## 0.1.2\n\n- Maintenance release; no user-facing changes.\n',
  )
})
