export type BuildMode = 'dev' | 'prod'

export interface BuildModeOptions {
  defaultMode?: BuildMode | undefined
}

export function resolveBuildMode(
  argv: readonly string[],
  options: BuildModeOptions = {},
): BuildMode {
  const dev = argv.includes('--dev')
  const prod = argv.includes('--prod')
  if (dev && prod) {
    throw new Error(
      'Conflicting build modes. Where: build arguments. Saw --dev and --prod; wanted one mode. Fix: select --dev or --prod.',
    )
  }
  return dev ? 'dev' : prod ? 'prod' : (options.defaultMode ?? 'dev')
}
