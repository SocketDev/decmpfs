/*
 * @file The ONE parser and formatter for pinned external artifact
 *   references. Labels are for humans; only content addresses are for
 *   machines: a full 40-hex git sha, or a content-addressed name whose final
 *   segment is a hex hash. On a pinned line the sha is the reference and the
 *   label is the comment - `<sha> # <label>` on hash-comment surfaces,
 *   `<sha> // <label>` on slash-comment surfaces - so tooling resolves the
 *   sha and never the label. Two label forms are accepted: a version tag
 *   such as `v3.2.1`, or a branch-plus-date stamp such as `main 2026-08-07`
 *   when no tag exists. The caller injects the date; nothing here reads the
 *   clock.
 */

export interface PinnedRef {
  label: string
  sha: string
}

export const FULL_SHA_LENGTH = 40

export const MIN_CONTENT_HASH_LENGTH = 7

/**
 * True when every character is a lowercase hex digit. Linear, and used
 * instead of a quantified character class so callers have no backtracking
 * surface.
 */
export function isLowercaseHex(text: string): boolean {
  for (let i = 0, { length } = text; i < length; i += 1) {
    const c = text.charCodeAt(i)
    const isDigit = c >= 48 && c <= 57
    const isAf = c >= 97 && c <= 102
    if (!isDigit && !isAf) {
      return false
    }
  }
  return text.length > 0
}

/**
 * True when every character is a decimal digit. Same linear shape as
 * {@link isLowercaseHex}.
 */
export function isDigitRun(text: string): boolean {
  for (let i = 0, { length } = text; i < length; i += 1) {
    const c = text.charCodeAt(i)
    if (c < 48 || c > 57) {
      return false
    }
  }
  return text.length > 0
}

/**
 * True when `text` is a full git object sha: exactly {@link FULL_SHA_LENGTH}
 * lowercase hex characters.
 */
export function isFullSha(text: string): boolean {
  return text.length === FULL_SHA_LENGTH && isLowercaseHex(text)
}

/**
 * True when a name is content-addressed by construction: its final `_`, `-`,
 * or `.` separated segment is a hex hash of {@link MIN_CONTENT_HASH_LENGTH}
 * or more characters, the `fleet-pack-<sha>` shape. A name with no separator
 * counts when the whole name is such a hash.
 */
export function isContentAddressedName(name: string): boolean {
  let start = -1
  for (let i = name.length - 1; i >= 0; i -= 1) {
    const c = name[i]!
    if (c === '_' || c === '-' || c === '.') {
      start = i + 1
      break
    }
  }
  const segment = start === -1 ? name : name.slice(start)
  return segment.length >= MIN_CONTENT_HASH_LENGTH && isLowercaseHex(segment)
}

/**
 * True when a label is a version tag such as `v3.2.1`: a literal `v`, then
 * dot-separated digit runs. Split-and-scan rather than a quantified group so
 * the check stays linear.
 */
export function isTagLabel(label: string): boolean {
  if (label[0] !== 'v') {
    return false
  }
  const segments = label.slice(1).split('.')
  for (let i = 0, { length } = segments; i < length; i += 1) {
    if (!isDigitRun(segments[i]!)) {
      return false
    }
  }
  return true
}

/**
 * True when a label is a branch-plus-date stamp such as `main 2026-08-07`: a
 * non-empty branch token, one space, an ISO calendar date.
 */
export function isBranchDateLabel(label: string): boolean {
  const parts = label.split(' ')
  if (parts.length !== 2) {
    return false
  }
  const branch = parts[0]!
  const date = parts[1]!
  // Anchored ISO calendar date: four year digits, a hyphen, two month
  // digits, a hyphen, two day digits. Fixed-width digit runs only, so
  // matching is linear.
  return branch.length > 0 && /^\d{4}-\d{2}-\d{2}$/.test(date)
}

/**
 * The branch-plus-date label for a pin with no tag. The caller injects the
 * date, never this function, so tests never depend on the clock.
 */
export function buildBranchLabel(branch: string, isoDate: string): string {
  return `${branch} ${isoDate}`
}

/**
 * The pinned line text for a surface: `<sha> # <label>` on a hash-comment
 * surface, `<sha> // <label>` on a slash-comment surface.
 */
export function formatPinComment(
  pin: PinnedRef,
  surface: 'hash' | 'slash',
): string {
  const marker = surface === 'hash' ? '#' : '//'
  return `${pin.sha} ${marker} ${pin.label}`
}

/**
 * The pin on a line: a full sha followed by ` # ` or ` // ` and a non-empty
 * label, or undefined when the line carries none. Leading content such as a
 * YAML key and trailing spaces are tolerated; only the token directly before
 * the comment marker is read as the sha.
 */
export function parsePinnedLine(line: string): PinnedRef | undefined {
  const markers = [' # ', ' // ']
  for (let m = 0, { length } = markers; m < length; m += 1) {
    const marker = markers[m]!
    let markerIndex = line.indexOf(marker)
    while (markerIndex !== -1) {
      const prefix = line.slice(0, markerIndex)
      const shaStart =
        Math.max(prefix.lastIndexOf(' '), prefix.lastIndexOf('\t')) + 1
      const sha = prefix.slice(shaStart)
      const label = line.slice(markerIndex + marker.length).trim()
      if (isFullSha(sha) && label.length > 0) {
        return { label, sha }
      }
      markerIndex = line.indexOf(marker, markerIndex + 1)
    }
  }
  return undefined
}
