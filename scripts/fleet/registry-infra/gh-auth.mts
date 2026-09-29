/*
 * @file One gh-auth preflight for every publish flow. `npm:`, `cargo:` and
 *   `github:` all reach GitHub before they reach a registry — a release is
 *   tagged, a release object is cut, a workflow is dispatched — so all three
 *   need an authenticated `gh` and all three used to discover that partway
 *   through, after a build had already run.
 *
 *   The preflight is a READ. It never logs anyone in on its own: `gh auth
 *   login` opens a browser and takes a device code, which is the operator's to
 *   drive, and a script that silently re-auths is a script that can be made to
 *   auth as somebody else. When the state is wrong this reports what is wrong
 *   and the one command that fixes it.
 *
 *   Token hygiene rides along, because the shape of the token matters as much
 *   as its presence (docs/fleet/agents.md/gh-token-hygiene.md): the fleet
 *   stores tokens in the OS keychain, never a plaintext config, and keeps the
 *   `workflow` scope OFF by default so a stray token cannot rewrite CI. A flow
 *   that dispatches a workflow is the one case that needs it, and it asks by
 *   name rather than the preflight granting it to everyone.
 *
 *   TWO ENVIRONMENTS, TWO ASSERTIONS. Keyring storage and OAuth scopes are
 *   facts about a DEVELOPER's standing login, and a runner has neither: gh
 *   reads `GH_TOKEN` from the environment, and a GitHub App installation token
 *   carries permissions rather than scopes, so `gh auth status` reports no
 *   keyring and no scopes for a token that is in every way correct. Demanding
 *   them there asserted nothing about the runner and blocked every CI flow.
 *
 *   The keyring rule exists because a developer's long-lived token must not
 *   sit in a world-readable dotfile. An installation token is minted for one
 *   job, expires in an hour, and never touches disk, so the reason does not
 *   reach it. What DOES matter on a runner is whether the token can perform
 *   the writes the flow is about to attempt, so that is what gets asserted:
 *   a live `GET /repos/{owner}/{repo}` whose `permissions.push` reports the
 *   `contents: write` grant a tag push and a release cut both need. That is a
 *   real read against the real token, not a skip.
 */

import { joinAnd } from '@socketsecurity/lib-stable/arrays/join'
import process from 'node:process'

import { spawnSync } from '@socketsecurity/lib-stable/process/spawn/child'

export interface GhAuthState {
  // Authenticated at all. Everything else is only meaningful when true.
  readonly authenticated: boolean
  // The login gh reports, for the "signed in as somebody else" case.
  readonly account: string | undefined
  // Stored in the OS keychain rather than a plaintext config file.
  readonly keyring: boolean
  // Scopes gh reports on the active token.
  readonly scopes: readonly string[]
  // Where gh says it read the active token from, verbatim from the
  // parenthetical it prints after the account name: `keyring`, the name of an
  // environment variable such as `GH_TOKEN`, or a config file path.
  readonly tokenSource: string | undefined
}

/**
 * What the token must be able to do for a tag push and a release cut: both
 * are `contents: write` on the repository, which GitHub reports back on the
 * repository object as `permissions.push`.
 */
export interface GhTokenCapability {
  // Operator-readable account of what the probe saw, for the failure block.
  readonly detail: string
  // The `owner/name` the probe asked about, or undefined when it could not be
  // resolved at all.
  readonly repo: string | undefined
  // GitHub answered that this token may write repository contents.
  readonly writesContents: boolean
}

/**
 * The environment variables gh reads a token from instead of its own config.
 *
 * A token arriving through one of these did not come off this machine's disk,
 * which is the whole distinction the keyring rule is drawn around.
 */
export const GH_ENV_TOKEN_VARS: readonly string[] = [
  'GH_ENTERPRISE_TOKEN',
  'GH_TOKEN',
  'GITHUB_ENTERPRISE_TOKEN',
  'GITHUB_TOKEN',
]

/**
 * Split `gh auth status` output into one chunk per logged-in account.
 *
 * Gh reports EVERY account it knows about, one block each, and a machine with
 * two logins on github.com gets two blocks. Pure.
 */
export function splitGhAccountBlocks(output: string): string[] {
  const lines = output.split(/\r?\n/)
  const blocks: string[] = []
  let current: string[] | undefined
  for (let i = 0, { length } = lines; i < length; i += 1) {
    const line = lines[i]!
    if (/Logged in to \S+ account \S+/.test(line)) {
      if (current) {
        blocks.push(current.join('\n'))
      }
      current = [line]
      continue
    }
    current?.push(line)
  }
  if (current) {
    blocks.push(current.join('\n'))
  }
  return blocks
}

/**
 * Parse `gh auth status` output. Pure, so the decision below is testable
 * without a `gh install` or a live token.
 *
 * Gh prints a human report rather than JSON here, so this reads the three
 * facts the fleet cares about and ignores the rest: whether a login is
 * reported at all, whether the token lives in the keyring, and the scope list.
 *
 * The ACTIVE account's block decides, never the first one printed. A machine
 * with two github.com logins gets two blocks, and gh does not promise the
 * active one comes first — so reading the file top-down attributed the wrong
 * account's scopes to the token in use. That is what made a login "succeed"
 * carrying scopes the token never had: the required scopes were sitting in the
 * OTHER block the whole time. When no block is marked active the first is
 * used, which is the single-account case and the only case where the two
 * readings agree.
 */
export function parseGhAuthStatus(output: string): GhAuthState {
  const blocks = splitGhAccountBlocks(output)
  const block =
    blocks.find(b => /Active account:\s*true/i.test(b)) ?? blocks[0] ?? ''
  const account = /Logged in to \S+ account (?<account>\S+)/.exec(block)
    ?.groups?.['account']
  // gh names the token's store in a parenthetical right after the account:
  // `(keyring)`, `(GH_TOKEN)`, or `(/path/to/hosts.yml)`. That one token is
  // the whole difference between a developer login and a runner token.
  const tokenSource = /Logged in to \S+ account \S+ \((?<source>[^)]*)\)/
    .exec(block)
    ?.groups?.['source']?.trim()
  const scopeLine =
    /Token scopes:\s*(?<scopes>.*)/.exec(block)?.groups?.['scopes'] ?? ''
  const scopes = scopeLine
    .split(',')
    // gh quotes each scope; strip one leading and one trailing quote.
    .map(s => s.trim().replace(/^['"]|['"]$/g, ''))
    .filter(Boolean)
  return {
    account,
    authenticated: !!account,
    // Read the parenthetical when there is one and fall back to the whole
    // block when a gh version prints a shape this does not know, so an
    // unparsed report can never read as keyring-backed by accident.
    keyring: /keyring/i.test(tokenSource ?? block),
    scopes,
    tokenSource,
  }
}

/**
 * The scopes in `required` that `state` does NOT carry, in the order asked
 * for. Pure, and the ONE place a "do we have it" question is answered — a
 * caller that re-derives this from a requested list is reporting its wish
 * rather than the token.
 */
export function missingScopes(
  state: { readonly scopes: readonly string[] },
  required: readonly string[],
): string[] {
  return required.filter(scope => !state.scopes.includes(scope))
}

/**
 * The four-ingredient block for a grant that reported success and did not
 * land. Pure.
 *
 * Prints BOTH sets, because the whole failure is that they were assumed
 * identical: gh's own success line names what was ASKED for, and a wrapper
 * that echoes it says "carrying workflow" over a token that carries no such
 * thing. Only the read-back list is evidence.
 */
export function formatScopeGrantMismatch(config: {
  granted: readonly string[]
  requested: readonly string[]
}): string {
  const cfg = { __proto__: null, ...config } as typeof config
  const short = cfg.requested.filter(s => !cfg.granted.includes(s))
  return [
    `the gh token did not come back carrying ${joinAnd(short)}.`,
    '  Where: `gh auth status`, re-read after the grant.',
    `  Saw vs. wanted: the token carries ${cfg.granted.length ? joinAnd(cfg.granted) : '(no scopes)'}; wanted ${joinAnd(cfg.requested)}.`,
    `  Fix: re-run \`pnpm run gh:auth scope:add ${short.join(' ')}\` and finish the approval in the browser.`,
  ].join('\n')
}

/**
 * What is wrong with `state` for a flow that needs `requiredScopes`, as
 * operator-readable lines, or an empty list when nothing is wrong. Pure.
 */
export function ghAuthProblems(
  state: GhAuthState,
  requiredScopes: readonly string[] = [],
): string[] {
  if (!state.authenticated) {
    return ['not signed in to GitHub']
  }
  const problems: string[] = []
  if (!state.keyring) {
    // A token in a plaintext config is readable by anything on the machine,
    // which is the whole reason the fleet requires the keyring.
    problems.push('the token is not stored in the OS keyring')
  }
  for (const scope of requiredScopes) {
    if (!state.scopes.includes(scope)) {
      problems.push(`the token is missing the ${scope} scope`)
    }
  }
  return problems
}

/**
 * True when gh is running off a token the ENVIRONMENT handed it on a CI
 * runner, rather than a login this machine stored. Pure given `env`.
 *
 * Both halves are required, and neither is enough alone. A developer who
 * exports `GH_TOKEN` in a shell profile is still a developer, and the keyring
 * rule still applies to them; a runner with a keyring-backed login is not a
 * shape that exists. Only the pair — a CI runner AND a token gh read out of
 * the environment — describes the ephemeral installation token this branch is
 * for, so the developer path cannot be reached by setting one variable.
 */
export function ghTokenIsRunnerSupplied(
  state: GhAuthState,
  env: NodeJS.ProcessEnv = process.env,
): boolean {
  if (!state.authenticated) {
    return false
  }
  const onRunner = env['GITHUB_ACTIONS'] === 'true' || env['CI'] === 'true'
  const source = state.tokenSource ?? ''
  return onRunner && GH_ENV_TOKEN_VARS.includes(source)
}

/**
 * The `owner/name` this flow is acting on: the runner's own
 * `GITHUB_REPOSITORY` when it is set, else whatever gh resolves from the
 * checkout's remotes.
 */
export function ghRepoSlug(
  env: NodeJS.ProcessEnv = process.env,
): string | undefined {
  const fromEnv = env['GITHUB_REPOSITORY']
  if (fromEnv) {
    return fromEnv
  }
  const run = spawnSync(
    'gh',
    ['repo', 'view', '--json', 'nameWithOwner', '--jq', '.nameWithOwner'],
    { encoding: 'utf8' },
  )
  const slug = String(run.stdout ?? '').trim()
  return slug || undefined
}

/**
 * Turn one `gh api repos/<slug> --jq .permissions.push` result into a verdict.
 * Pure, so every branch is testable without a token or a network.
 *
 * An unrecognized answer is a REFUSAL, never a pass. The failure this replaces
 * was a preflight that asserted something no runner could satisfy; replacing
 * it with one that shrugs at an answer it does not understand would be the
 * same mistake pointed the other way.
 */
export function parseGhContentsCapability(config: {
  code: number
  output: string
  repo: string
}): GhTokenCapability {
  const cfg = { __proto__: null, ...config } as typeof config
  const answer = cfg.output.trim()
  if (cfg.code !== 0) {
    const firstLine = answer.split(/\r?\n/)[0] ?? ''
    return {
      detail:
        `the token could not read ${cfg.repo} (gh api exited ${cfg.code}` +
        `${firstLine ? `: ${firstLine}` : ''})`,
      repo: cfg.repo,
      writesContents: false,
    }
  }
  if (answer === 'true') {
    return {
      detail: `the token carries contents: write on ${cfg.repo}`,
      repo: cfg.repo,
      writesContents: true,
    }
  }
  if (answer === 'false') {
    return {
      detail:
        `the token authenticates against ${cfg.repo} but is READ-ONLY there ` +
        `(permissions.push is false), so it can create neither a tag nor a release`,
      repo: cfg.repo,
      writesContents: false,
    }
  }
  return {
    detail:
      `GitHub reported no permissions.push for ${cfg.repo} (saw ` +
      `${answer ? JSON.stringify(answer) : '(empty)'}), so the token's write ` +
      `access could not be established`,
    repo: cfg.repo,
    writesContents: false,
  }
}

/**
 * Ask GitHub, with the token actually in hand, whether it may write this
 * repository's contents.
 *
 * `permissions.push` is the field a GitHub App installation token's
 * `contents: write` grant surfaces on the repository object, and `contents:
 * write` is exactly the grant a tag ref and a release both require. One
 * authenticated read therefore proves three things at once: the token is
 * valid, it reaches THIS repository, and it is not read-only.
 */
export function readGhContentsCapability(
  options?: { readonly repo?: string | undefined } | undefined,
): GhTokenCapability {
  const { repo = ghRepoSlug() } = {
    __proto__: null,
    ...options,
  } as { repo?: string | undefined }
  if (!repo) {
    return {
      detail:
        'the repository could not be resolved (no GITHUB_REPOSITORY and no gh remote), ' +
        "so the token's write access could not be checked",
      repo: undefined,
      writesContents: false,
    }
  }
  const run = spawnSync(
    'gh',
    ['api', `repos/${repo}`, '--jq', '.permissions.push'],
    { encoding: 'utf8' },
  )
  return parseGhContentsCapability({
    code: run.status ?? 1,
    output: `${String(run.stdout ?? '')}\n${String(run.stderr ?? '')}`,
    repo,
  })
}

/**
 * What is wrong with a RUNNER's gh state, as operator-readable lines, or an
 * empty list when nothing is wrong. Pure.
 *
 * Deliberately says nothing about keyring storage or OAuth scopes: neither is
 * a property an installation token can have. The capability probe is the whole
 * assertion, and it is a stronger one — a keyring-stored login carrying every
 * scope in the catalogue still cannot tag a repository it has no write access
 * to, and this catches that where the scope list never would.
 */
export function ghRunnerAuthProblems(
  state: GhAuthState,
  capability: GhTokenCapability,
): string[] {
  if (!state.authenticated) {
    return ['not signed in to GitHub']
  }
  return capability.writesContents ? [] : [capability.detail]
}

/**
 * The Fix line for a runner whose token cannot write: the token is minted
 * from an App installation, so the repair is on the installation's grant, not
 * on anything the workflow can pass differently.
 */
export const GH_APP_GRANT_FIX =
  'the release App installation must grant `contents: write` on this repository — ' +
  'check the App settings (Repository access + Permissions), then re-run; a workflow ' +
  'flag cannot widen a token past its installation grant'

/**
 * The Fix line for a flow that wants the fleet's own login wrapper rather
 * than a raw gh command — keyring, ssh, and the standing scope set in one.
 */
export const GH_LOGIN_FIX =
  'pnpm run gh:auth login (wraps the browser flow with the fleet flag set)'

/**
 * The four-ingredient block for a failed preflight, naming the flow so an
 * operator knows which command stopped and why. Pure.
 *
 * `fix` overrides the prescribed command. A flow that BORROWS its scopes
 * needs it: such a flow requires no scope of the standing login, so the
 * scope-derived heuristic would prescribe a raw `gh auth login` and route the
 * operator around the wrapper that sets the fleet flags.
 *
 * `wanted` overrides the right-hand side of the Saw-vs-wanted line. A runner
 * is not short a keyring-stored login and never will be, so telling it so
 * points the reader at a repair that does not exist.
 */
export function formatGhAuthFailure(config: {
  fix?: string | undefined
  flow: string
  problems: readonly string[]
  requiredScopes: readonly string[]
  wanted?: string | undefined
}): string {
  const cfg = { __proto__: null, ...config } as typeof config
  const scopeFlag = cfg.requiredScopes.length
    ? ` --scopes ${cfg.requiredScopes.join(',')}`
    : ''
  const fix =
    cfg.fix ??
    (cfg.requiredScopes.includes('workflow')
      ? GH_LOGIN_FIX
      : `gh auth login --hostname github.com --git-protocol ssh${scopeFlag} --web`)
  const wanted =
    cfg.wanted ??
    `a keyring-stored login${
      cfg.requiredScopes.length
        ? ` carrying ${joinAnd(cfg.requiredScopes)}`
        : ''
    }`
  return [
    `${cfg.flow} needs an authenticated gh before it starts.`,
    `  Where: the gh auth preflight, before anything is built or published.`,
    `  Saw vs. wanted: ${cfg.problems.join('; ')}; wanted ${wanted}.`,
    `  Fix: ${fix}`,
  ].join('\n')
}

/**
 * Read the current gh auth state. Returns an unauthenticated state when gh is
 * absent or errors, so a caller decides what to do rather than crashing here.
 */
export function readGhAuthState(): GhAuthState {
  const run = spawnSync('gh', ['auth', 'status'], { encoding: 'utf8' })
  // gh writes the report to stderr on some versions and stdout on others.
  const output = `${String(run.stdout ?? '')}\n${String(run.stderr ?? '')}`
  return parseGhAuthStatus(output)
}

/**
 * Throw with the four-ingredient block unless gh is authenticated the way
 * `flow` needs, in the way the CURRENT environment can be authenticated. The
 * one call every publish flow makes first.
 *
 * Two branches, one question. A developer is asked for keyring storage and the
 * scopes the flow named, because those are the facts that describe a standing
 * login and the ways it goes wrong. A runner is asked whether its token can
 * actually write this repository's contents, because that is the fact that
 * describes an installation token and the only way THAT goes wrong. Neither
 * branch waves the other through: the runner branch performs a live
 * authenticated read and refuses on anything short of a `true`.
 */
export function assertGhAuth(config: {
  /** The caller minted this token through the release-app action, whose own preflight verified the installation grant. The repo-object push flag false-negatives for app installation tokens. */
  appGrantVerified?: boolean | undefined
  fix?: string | undefined
  flow: string
  requiredScopes?: readonly string[] | undefined
}): void {
  const cfg = { __proto__: null, ...config } as typeof config
  const requiredScopes = cfg.requiredScopes ?? []
  const state = readGhAuthState()
  if (ghTokenIsRunnerSupplied(state)) {
    if (cfg.appGrantVerified) return
    const capability = readGhContentsCapability()
    const problems = ghRunnerAuthProblems(state, capability)
    if (problems.length) {
      throw new Error(
        formatGhAuthFailure({
          fix: GH_APP_GRANT_FIX,
          flow: cfg.flow,
          problems,
          requiredScopes: [],
          wanted: `a token that can write ${capability.repo ?? 'this repository'}'s contents (the grant a tag push and a release cut both need)`,
        }),
      )
    }
    return
  }
  const problems = ghAuthProblems(state, requiredScopes)
  if (problems.length) {
    throw new Error(
      formatGhAuthFailure({
        fix: cfg.fix,
        flow: cfg.flow,
        problems,
        requiredScopes,
      }),
    )
  }
}

/**
 * `gh auth refresh` argv that grants or drops one scope. Pure, so a scope
 * expand/reduce is checkable without a `gh install`.
 *
 * One scope per call by design: `gh auth refresh` is an interactive device
 * flow, so a caller wanting two scopes runs this twice rather than building
 * a combined flag the operator cannot audit as easily.
 */
export function refreshScopeArgs(config: {
  readonly add?: string | undefined
  readonly remove?: string | undefined
}): string[] {
  const cfg = { __proto__: null, ...config } as typeof config
  const args = ['auth', 'refresh', '-h', 'github.com']
  if (cfg.add) {
    args.push('-s', cfg.add)
  }
  if (cfg.remove) {
    args.push('-r', cfg.remove)
  }
  return args
}
