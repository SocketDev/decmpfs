// Print the CHANGELOG.md section body for a version — the release notes for
// `gh release create`. Dep-0 so CI runs it with no install.
//
//   node scripts/repo/changelog-section.mts 0.2.0

import { readFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', '..')

function main(): void {
  const version = (process.argv[2] ?? '').replace(/^v/, '')
  if (!version) {
    process.stderr.write(
      'usage: node scripts/repo/changelog-section.mts <version>\n' +
        '  where: argv[2]. saw: nothing. fix: pass the version, e.g. 0.2.0.\n',
    )
    process.exit(1)
  }

  const changelog = readFileSync(path.join(root, 'CHANGELOG.md'), 'utf8')
  const lines = changelog.split('\n')
  // A section runs from its `## <version>` heading to the next `## ` heading.
  const start = lines.findIndex(l => l.trim() === `## ${version}`)
  if (start === -1) {
    process.stderr.write(
      `no "## ${version}" section in CHANGELOG.md. Fix: add the section before ` +
        `tagging v${version}.\n`,
    )
    process.exit(1)
  }
  let end = lines.length
  for (let i = start + 1, { length } = lines; i < length; i += 1) {
    if (lines[i]!.startsWith('## ')) {
      end = i
      break
    }
  }
  process.stdout.write(
    lines
      .slice(start + 1, end)
      .join('\n')
      .trim() + '\n',
  )
}

main()
