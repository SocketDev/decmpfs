use decmpfs::{Gate, Outcome, Support, UnsupportedReason};
use std::path::{Path, PathBuf};

#[derive(Default)]
struct Summary {
    candidates: u64,
    compressed: u64,
    no_gain: u64,
    already_compressed: u64,
    unsupported: u64,
    skipped: u64,
    bytes_before: u64,
    bytes_after: u64,
}

impl Summary {
    fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Compressed { before, after } => {
                self.compressed += 1;
                self.bytes_before += before;
                self.bytes_after += after;
            }
            Outcome::NoGain { before, after } => {
                self.no_gain += 1;
                self.bytes_before += before;
                self.bytes_after += after;
            }
            Outcome::AlreadyCompressed { before } => {
                self.already_compressed += 1;
                self.bytes_before += before;
                self.bytes_after += before;
            }
            Outcome::Unsupported { .. } => self.unsupported += 1,
            Outcome::Skipped { .. } => self.skipped += 1,
        }
    }

    fn print(&self, dry_run: bool) {
        if dry_run {
            println!(
                "{} eligible files (dry run; no compression estimate)",
                self.candidates
            );
            return;
        }
        println!(
            "{} eligible; {} compressed, {} no gain, {} already compressed, {} unsupported, {} skipped",
            self.candidates,
            self.compressed,
            self.no_gain,
            self.already_compressed,
            self.unsupported,
            self.skipped
        );
        let saved = self.bytes_before.saturating_sub(self.bytes_after);
        println!(
            "measured files: {} bytes before, {} after, {} saved",
            self.bytes_before, self.bytes_after, saved
        );
    }
}

fn collect_files(
    root: &Path,
    current: &Path,
    gate: &Gate,
    files: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(current)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        let relative = current
            .strip_prefix(root)
            .unwrap_or(current)
            .to_string_lossy()
            .replace('\\', "/");
        if gate.matches(&relative, metadata.len()) {
            files.push(current.to_path_buf());
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    let mut entries = std::fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        collect_files(root, &entry.path(), gate, files)?;
    }
    Ok(())
}

fn run(root: &Path, gate: &Gate, dry_run: bool) -> Result<Summary, String> {
    let metadata =
        std::fs::symlink_metadata(root).map_err(|error| format!("{}: {error}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{} must be a real directory", root.display()));
    }

    match decmpfs::probe(root).map_err(|error| format!("{}: {error}", root.display()))? {
        Support::Supported | Support::AlreadyCompressed => {}
        Support::Unsupported(UnsupportedReason::Filesystem) => {
            return Err(format!(
                "{} is on a filesystem without per-file transparent compression",
                root.display()
            ));
        }
        Support::Unsupported(reason) => {
            return Err(format!("{} is unsupported: {reason:?}", root.display()));
        }
    }

    let mut files = Vec::new();
    collect_files(root, root, gate, &mut files)
        .map_err(|error| format!("walking {}: {error}", root.display()))?;
    files.sort();

    let mut summary = Summary {
        candidates: files.len() as u64,
        ..Summary::default()
    };
    if !dry_run {
        for path in files {
            let outcome = decmpfs::compress_file(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            summary.record(outcome);
        }
    }
    Ok(summary)
}

fn usage() -> &'static str {
    "Usage: decmpfs <directory> [-g GLOB] [-s SIZE] [-n]\n\nCompress eligible regular files on a supported filesystem. Symlinks are not followed."
}

#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
    root: PathBuf,
    glob: Option<String>,
    min_size: Option<String>,
    dry_run: bool,
}

fn parse_args(args: &[String]) -> Result<CliOptions, String> {
    let Some(root) = args.first() else {
        return Err(usage().to_string());
    };
    let mut options = CliOptions {
        root: PathBuf::from(root),
        glob: None,
        min_size: None,
        dry_run: false,
    };
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "-g" | "--glob" | "-s" | "--min-size" => {
                let option = args[index].as_str();
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(format!("{option} requires a value\n{}", usage()));
                };
                if option == "-g" || option == "--glob" {
                    options.glob = Some(value.clone());
                } else {
                    options.min_size = Some(value.clone());
                }
            }
            "-n" | "--dry-run" => options.dry_run = true,
            unknown => return Err(format!("unknown option: {unknown}\n{}", usage())),
        }
        index += 1;
    }
    Ok(options)
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{}", usage());
        return;
    }
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    let gate = match Gate::new(options.glob.as_deref(), options.min_size.as_deref()) {
        Ok(gate) => gate,
        Err(error) => {
            eprintln!("invalid gate: {error}");
            std::process::exit(2);
        }
    };
    match run(&options.root, &gate, options.dry_run) {
        Ok(summary) => summary.print(options.dry_run),
        Err(error) => {
            eprintln!("decmpfs: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decmpfs::SkipReason;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("decmpfs-tree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn cli_accepts_a_directory_with_short_filter_flags() {
        let parsed =
            parse_args(&args(&["target", "-g", "**/*.rlib", "-s", ">=1MiB", "-n"])).unwrap();
        assert_eq!(parsed.root, PathBuf::from("target"));
        assert_eq!(parsed.glob.as_deref(), Some("**/*.rlib"));
        assert_eq!(parsed.min_size.as_deref(), Some(">=1MiB"));
        assert!(parsed.dry_run);
    }

    #[test]
    fn cli_accepts_long_filter_flags_and_defaults_to_compress() {
        assert_eq!(
            parse_args(&args(&["target"])).unwrap(),
            CliOptions {
                root: PathBuf::from("target"),
                glob: None,
                min_size: None,
                dry_run: false,
            }
        );
        let parsed = parse_args(&args(&[
            "target",
            "--glob",
            "**/*.rlib",
            "--min-size",
            ">=1MiB",
            "--dry-run",
        ]))
        .unwrap();
        assert_eq!(parsed.glob.as_deref(), Some("**/*.rlib"));
        assert_eq!(parsed.min_size.as_deref(), Some(">=1MiB"));
        assert!(parsed.dry_run);
    }

    #[test]
    fn cli_rejects_missing_directory_values_and_unknown_options() {
        assert!(parse_args(&[]).unwrap_err().contains("Usage: decmpfs"));
        assert!(parse_args(&args(&["target", "-g"]))
            .unwrap_err()
            .contains("-g requires a value"));
        assert!(parse_args(&args(&["target", "--mystery"]))
            .unwrap_err()
            .contains("unknown option: --mystery"));
    }

    #[test]
    fn tree_walk_applies_relative_glob_and_size_gate_without_following_symlinks() {
        let root = scratch("walk");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/cache.bin"), vec![1; 16]).unwrap();
        std::fs::write(root.join("nested/small.bin"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("nested"), root.join("linked")).unwrap();

        let gate = Gate::new(Some("**/*.bin"), Some(">= 8B")).unwrap();
        let mut files = Vec::new();
        collect_files(&root, &root, &gate, &mut files).unwrap();
        files.sort();
        assert_eq!(files, vec![root.join("nested/cache.bin")]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn summary_counts_only_verified_space_reduction() {
        let mut summary = Summary::default();
        summary.record(Outcome::Compressed {
            before: 100,
            after: 40,
        });
        summary.record(Outcome::NoGain {
            before: 12,
            after: 12,
        });
        summary.record(Outcome::Skipped {
            reason: SkipReason::PermissionDenied,
        });
        assert_eq!(summary.compressed, 1);
        assert_eq!(summary.no_gain, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.bytes_before.saturating_sub(summary.bytes_after), 60);
    }
}
