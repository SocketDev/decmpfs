# decmpfs commands

The `decmpfs` executable compresses eligible regular files under one directory. It does not follow symlinks. Run it after a build or install has finished so it does not rewrite files that another process is using.

```sh
decmpfs DIRECTORY [-g GLOB] [-s SIZE] [-n]
```

| Option                  | Meaning                                                                      |
| ----------------------- | ---------------------------------------------------------------------------- |
| `DIRECTORY`             | Root directory to scan. This positional argument is required.                |
| `-g`, `--glob GLOB`     | Compress only paths that match the glob.                                     |
| `-s`, `--min-size SIZE` | Compress only files that meet the size predicate, such as `>=1MiB`.          |
| `-n`, `--dry-run`       | Report eligible files without writing compressed data or estimating savings. |
| `-h`, `--help`          | Print usage and exit successfully.                                           |

Example:

```sh
cargo install decmpfs --bin decmpfs
decmpfs target -g '**/*.rlib' -s '>=1MiB' --dry-run
decmpfs target -g '**/*.rlib' -s '>=1MiB'
```

The command fails before scanning when the filesystem has no supported compression backend. Invalid options or gate expressions exit with status `2`. A successful compression may still report unsupported or skipped files; those are non-fatal per-file outcomes. The command reports verified allocation changes after a write.

The default install provides the Rust library and command. Enable the `addon` feature when you need to unwrap a compressed napi hybrid `.node` file before writing it. Enable the `exe` feature only for executable packing.
