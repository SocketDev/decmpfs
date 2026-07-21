// Type-only fixture: `pnpm run typecheck` (tsc 7 native) compiles this against
// index.d.ts. It imports EVERY public export and uses it at the declared types,
// so a wrong or missing declaration fails the type-check; the `@ts-expect-error`
// lines assert the declarations reject misuse. Not executed — no runtime side
// effects — so the negative lines never run.

import {
  copyDecmpfsFile,
  copyDecmpfsFileSync,
  copyFile,
  COPYFILE_EXCL,
  COPYFILE_FICLONE,
  COPYFILE_FICLONE_FORCE,
  copyFileSync,
  createDecmpfsWriteStream,
  packExecutable,
  packExecutableSync,
  writeDecmpfsFile,
  writeDecmpfsFileSync,
} from '../index.cjs'
import type { DecmpfsResult, PackExeResult } from '../index.cjs'

export async function surface(): Promise<void> {
  const consts: number =
    COPYFILE_EXCL + COPYFILE_FICLONE + COPYFILE_FICLONE_FORCE
  void consts

  const w: DecmpfsResult = writeDecmpfsFileSync('/tmp/a', new Uint8Array([1]), {
    force: true,
    errorOnExist: false,
    atomic: true,
    glob: '**/*.node',
    minSize: '>= 1MB',
  })
  void (w.compressed && w.before + w.after > 0 && w.reason.length)
  const wa: DecmpfsResult = await writeDecmpfsFile(
    '/tmp/a',
    new Uint8Array([1]),
  )
  void wa

  const stream = createDecmpfsWriteStream('/tmp/model.bin', {
    size: 1024,
    glob: '**/*.bin',
  })
  stream.write(new Uint8Array([1]))
  stream.end()
  const streamResult: DecmpfsResult | undefined = stream.result
  void streamResult

  const c: DecmpfsResult = copyDecmpfsFileSync('/a', '/b', { force: false })
  void (await copyDecmpfsFile('/a', '/b', { errorOnExist: true }))
  void c

  const cf: DecmpfsResult = copyFileSync('/a', '/b', COPYFILE_EXCL)
  void (await copyFile('/a', '/b'))
  void cf

  const p: PackExeResult = packExecutableSync('/src', '/dest', {
    stub: '/stub',
    gateGlob: '**/*',
    gateSize: '>= 1MB',
  })
  void (p.packed && p.skippedGate === false && p.before + p.after >= 0)
  void (await packExecutable('/src', '/dest', { stub: '/stub' }))

  // packExecutable requires `stub` — omitting it is a type error.
  // @ts-expect-error stub is required
  packExecutableSync('/src', '/dest', {})

  // data must be bytes, not a string.
  // @ts-expect-error data is Uint8Array, not string
  writeDecmpfsFileSync('/tmp/a', 'not-bytes')

  // reason is a string; treating it as a number is an error.
  // @ts-expect-error reason is a string
  const bad: number = c.reason
  void bad
}

void surface
