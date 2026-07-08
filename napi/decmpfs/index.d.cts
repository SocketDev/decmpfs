// Type declarations for the decmpfs native addon (index.cjs re-exports the
// prebuilt `.node`). Hand-maintained in lockstep with the #[napi] surface in
// src/lib.rs — this repo hand-rolls the loader + build rather than using the
// napi CLI codegen. A drift test (test/dts.test.mts) asserts every name
// declared here is actually exported by the addon.

/** Node's `fs.copyFile` mode flag: fail if the destination exists. */
export const COPYFILE_EXCL: number
/** Node's `fs.copyFile` mode flag: clone (reflink) when the filesystem can. */
export const COPYFILE_FICLONE: number
/** Node's `fs.copyFile` mode flag: require a clone, else fail. */
export const COPYFILE_FICLONE_FORCE: number

/** The outcome of a write/copy. Only a thrown error is a hard failure. */
export interface DecmpfsResult {
  /** Whether the file landed OS-compressed (false = wrote plain). */
  compressed: boolean
  /** Logical size of the content written. */
  before: number
  /** On-disk allocated size after the write. */
  after: number
  /**
   * Outcome category: `Compressed` / `NoGain` / `AlreadyCompressed` /
   * `Unsupported:*` / `Skipped:*` / `ExistsNoForce`.
   */
  reason: string
}

/** Options for {@link writeDecmpfsFile} / {@link writeDecmpfsFileSync}. */
export interface WriteDecmpfsOptions {
  /** Replace an existing destination (default true). */
  force?: boolean
  /** Throw if the destination exists (default false). */
  errorOnExist?: boolean
  /** Write atomically via a temp file + rename (default true). */
  atomic?: boolean
  /** Only compress when the path matches this glob. */
  glob?: string
  /** Only compress when the content is at least this size (e.g. `>= 1MB`). */
  minSize?: string
}

/** Options for {@link copyDecmpfsFile} / {@link copyDecmpfsFileSync}. */
export interface CopyDecmpfsOptions {
  /** Replace an existing destination (default true). */
  force?: boolean
  /** Throw if the destination exists (default false). */
  errorOnExist?: boolean
}

/** Options for {@link rm} / {@link rmSync} — exactly Node's `fs.rm` options. */
export interface RmOptions {
  /** Recursive removal (`rm -rf` with `force`). Default false. */
  recursive?: boolean
  /**
   * Ignore a missing path AND bypass the safe-delete guard (refusing to remove
   * the cwd, an ancestor of it, or the filesystem root). Default false.
   */
  force?: boolean
  /**
   * Retries on EBUSY/EMFILE/ENFILE/ENOTEMPTY/EPERM (recursive only). Default 0.
   */
  maxRetries?: number
  /** Milliseconds between retries, linear backoff (recursive only). Default 100. */
  retryDelay?: number
}

/** Options for {@link packExecutable} / {@link packExecutableSync}. */
export interface PackExeOptions {
  /**
   * Path to the self-replacing stub binary the payload is injected into (a
   * decmpfs-stub build, or any executable that calls the crate's
   * self_replace_and_exec). Required — the Node host is not a self-replacing
   * runtime, so there is no sensible default.
   */
  stub: string
  /** Gate glob (e.g. `**\/*.node`). Default: match any path. */
  gateGlob?: string
  /** Gate size predicate (e.g. `>= 1MB`). Default: no size floor. */
  gateSize?: string
}

/** The outcome of packing an executable — a success shape (never thrown for a gate miss). */
export interface PackExeResult {
  /** Whether the executable was packed (false = the gate excluded it). */
  packed: boolean
  /** Logical size of the source executable (0 on a gate miss). */
  before: number
  /** On-disk size of the packed stub (0 on a gate miss). */
  after: number
  /** Whether the gate rejected the input — nothing was read or written. */
  skippedGate: boolean
}

/** Write `data` to `path` as an OS-compressed file in one pass. */
export function writeDecmpfsFileSync(
  path: string,
  data: Uint8Array,
  options?: WriteDecmpfsOptions,
): DecmpfsResult
/** Async {@link writeDecmpfsFileSync}. */
export function writeDecmpfsFile(
  path: string,
  data: Uint8Array,
  options?: WriteDecmpfsOptions,
): Promise<DecmpfsResult>

/** Copy `src` to `dest`, preserving OS compression (clone or recompress). */
export function copyDecmpfsFileSync(
  src: string,
  dest: string,
  options?: CopyDecmpfsOptions,
): DecmpfsResult
/** Async {@link copyDecmpfsFileSync}. */
export function copyDecmpfsFile(
  src: string,
  dest: string,
  options?: CopyDecmpfsOptions,
): Promise<DecmpfsResult>

/**
 * `fsPromises.copyFile`-shaped, decmpfs-aware. `mode` is a bitmask of the
 * `COPYFILE_*` flags; the clone path works on macOS, where libuv's does not.
 */
export function copyFileSync(
  src: string,
  dest: string,
  mode?: number,
): DecmpfsResult
/** Async {@link copyFileSync}. */
export function copyFile(
  src: string,
  dest: string,
  mode?: number,
): Promise<DecmpfsResult>

/**
 * `fs.rmSync(path, options)` — decmpfs-aware, with a safe-delete guard that
 * refuses to remove the cwd, an ancestor of it, or the filesystem root unless
 * `force`. Errors match Node's fs shape (`code`/`errno`/`syscall`/`path`).
 */
export function rmSync(path: string, options?: RmOptions): void
/** Async {@link rmSync} (`fsPromises.rm`). */
export function rm(path: string, options?: RmOptions): Promise<void>

/**
 * Turn an existing file into an OS-FS-compressed file IN PLACE (atomic rewrite).
 * The bytes are unchanged to every reader; only the on-disk representation
 * changes — a `chmod`-for-compression.
 */
export function compressFileSync(path: string): DecmpfsResult
/** Async {@link compressFileSync}. */
export function compressFile(path: string): Promise<DecmpfsResult>

/** Pack `src` into a self-replacing executable at `dest` using `options.stub`. */
export function packExecutableSync(
  src: string,
  dest: string,
  options: PackExeOptions,
): PackExeResult
/** Async {@link packExecutableSync}. */
export function packExecutable(
  src: string,
  dest: string,
  options: PackExeOptions,
): Promise<PackExeResult>

// napi async-task handles — exported by the addon as an artifact of the
// AsyncTask bindings, but not part of the public API. The async functions above
// resolve to the result objects; consumers never construct or touch these.
export declare class WriteTask {}
export declare class CopyTask {}
export declare class CopyFileTask {}
export declare class PackExeTask {}
export declare class RmTask {}
export declare class CompressFileTask {}
