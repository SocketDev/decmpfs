/*
 * @file Mint a short-lived GitHub App installation token for the decmpfs
 * release app. Dep-0 (node: builtins only) so it runs in CI before any install;
 * github-release.yml sets up Node 24, which strips its .mts types natively. RS256 JWT
 * (iss = the app Client ID) -> the repo installation -> an installation token
 * scoped by PERMISSIONS. The token is masked, then handed back via
 * $GITHUB_OUTPUT.
 *
 * The repo-level installation lookup (`/repos/{owner}/{repo}/installation`)
 * works whether the app is installed on the whole org/account or just this
 * repo, so it needs no org-vs-user branching.
 *
 * Env:
 *   CLIENT_ID       (required) the GitHub App Client ID
 *   APP_PRIVATE_KEY (required) the app private key (PEM)
 *   REPOSITORY      (required) `owner/repo` (pass github.repository)
 *   PERMISSIONS     (optional) JSON object, e.g. {"contents":"write"}; an empty
 *                              object is rejected (would mint blanket perms)
 *   GITHUB_OUTPUT   (required) set by the runner; token is written here.
 */

import crypto from 'node:crypto'
import { appendFileSync } from 'node:fs'
import { request } from 'node:https'
import process from 'node:process'
import { pathToFileURL } from 'node:url'

function die(message: string): never {
  process.stderr.write(`[mint-app-token] ${message}\n`)
  process.exit(1)
}

function env(name: string): string {
  const value = process.env[name]
  if (!value) {
    die(
      `required env ${name} is not set. Where: the release-app-token action's ` +
        `env block. Fix: pass ${name} (CLIENT_ID/REPOSITORY from inputs, ` +
        `APP_PRIVATE_KEY from the secret).`,
    )
  }
  return value
}

interface GhResponse {
  body: string
  status: number
}

function gh(
  method: string,
  path: string,
  jwt: string,
  body?: string,
): Promise<GhResponse> {
  const headers: Record<string, string> = {
    accept: 'application/vnd.github+json',
    authorization: `Bearer ${jwt}`,
    'user-agent': 'decmpfs-release-app-token',
    'x-github-api-version': '2022-11-28',
  }
  if (body !== undefined) {
    headers['content-length'] = String(Buffer.byteLength(body))
    headers['content-type'] = 'application/json'
  }
  return new Promise<GhResponse>((resolve, reject) => {
    const req = request(
      { headers, host: 'api.github.com', method, path, port: 443 },
      res => {
        const chunks = []
        res.on('data', chunk => chunks.push(chunk))
        res.on('end', () =>
          resolve({
            body: Buffer.concat(chunks).toString('utf8'),
            status: res.statusCode ?? 0,
          }),
        )
      },
    )
    req.setTimeout(15_000, () =>
      req.destroy(new Error(`${method} ${path} timed out`)),
    )
    req.on('error', reject)
    if (body !== undefined) {
      req.write(body)
    }
    req.end()
  })
}

// Parse a PERMISSIONS string (a JSON object) into the access-token request, or
// undefined when blank. Throws on malformed or empty-object input — an empty
// object would mint a blanket-permission token, the opposite of least-privilege.
// Pure + exported so it is unit-testable.
export function parsePermissions(
  rawInput: string | undefined,
): Record<string, string> | undefined {
  const raw = rawInput?.trim()
  if (!raw) {
    return undefined
  }
  let parsed
  try {
    parsed = JSON.parse(raw)
  } catch {
    throw new Error(
      `PERMISSIONS is not valid JSON. Saw: ${raw}. ` +
        `Fix: pass a JSON object like {"contents":"write"}.`,
    )
  }
  if (
    typeof parsed !== 'object' ||
    parsed === null ||
    Array.isArray(parsed) ||
    Object.keys(parsed).length === 0
  ) {
    throw new Error(
      `PERMISSIONS must be a non-empty JSON object. Saw: ${raw}. ` +
        `Fix: pass e.g. {"contents":"write"}; an empty object would mint a ` +
        `blanket-permission token.`,
    )
  }
  return parsed
}

async function main(): Promise<void> {
  const clientId = env('CLIENT_ID')
  const privateKey = env('APP_PRIVATE_KEY')
  const repository = env('REPOSITORY')
  const permissions = parsePermissions(process.env['PERMISSIONS'])
  const now = Math.floor(Date.now() / 1000)
  const head = Buffer.from(
    JSON.stringify({ alg: 'RS256', typ: 'JWT' }),
  ).toString('base64url')
  const claims = Buffer.from(
    JSON.stringify({ exp: now + 540, iat: now - 60, iss: clientId }),
  ).toString('base64url')
  const signature = crypto
    .createSign('RSA-SHA256')
    .update(`${head}.${claims}`)
    .sign(privateKey, 'base64url')
  const jwt = `${head}.${claims}.${signature}`

  const inst = await gh('GET', `/repos/${repository}/installation`, jwt)
  if (inst.status !== 200) {
    die(
      `installation lookup failed: HTTP ${inst.status}. Where: GET ` +
        `/repos/${repository}/installation. Saw: ${inst.body}. Fix: confirm the ` +
        `app (CLIENT_ID) is installed on ${repository}.`,
    )
  }
  const installationId = JSON.parse(inst.body).id
  if (typeof installationId !== 'number') {
    die(`installation lookup returned no id. Saw: ${inst.body}.`)
  }

  const tokenBody = {}
  if (permissions !== undefined) {
    tokenBody.permissions = permissions
  }
  const minted = await gh(
    'POST',
    `/app/installations/${installationId}/access_tokens`,
    jwt,
    JSON.stringify(tokenBody),
  )
  if (minted.status !== 201) {
    die(
      `token mint failed: HTTP ${minted.status}. Where: POST ` +
        `/app/installations/${installationId}/access_tokens. Saw: ${minted.body}. ` +
        `Fix: the requested permissions must be a subset of what the app's ` +
        `installation grants (a 422 means the install lacks a requested scope).`,
    )
  }
  const token = JSON.parse(minted.body).token
  if (!token) {
    die(`token mint returned no token. Saw: ${minted.body}.`)
  }

  process.stdout.write(`::add-mask::${token}\n`)
  appendFileSync(env('GITHUB_OUTPUT'), `token=${token}\n`)
}

// Guard the entry IIFE so importing the module (unit tests import parsePermissions)
// does NOT run main(). Run only when invoked directly as the script.
if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  void (async () => {
    try {
      await main()
    } catch (e) {
      die(e instanceof Error ? e.message : String(e))
    }
  })()
}
