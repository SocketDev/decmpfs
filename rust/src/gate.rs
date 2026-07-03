//! The install-time gate: decide whether a given file should be OS-compressed.
//!
//! A `Gate` is a glob AND/OR a size predicate. The PM helper calls
//! `gate.matches(name, len)` after it knows the addon's name + decoded length and
//! only hands the bytes to `compress_bytes` when both predicates pass. Both halves
//! are optional; a `Gate::default()` matches the fleet default `**/*.node` with no
//! size floor.
//!
//! The size predicate parses a human string (`">"`/`">="` + a number with an
//! optional unit) so a manifest can carry `compress = ">= 1MB"` verbatim. Units are
//! case-insensitive and cover both the decimal (`KB`/`MB`/`GB` = 1000ⁿ) and binary
//! (`KiB`/`MiB`/`GiB` = 1024ⁿ) families; a bare number is bytes.

/// A `>`/`>=` comparison against a byte threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizePredicate {
  /// `len > threshold`.
  GreaterThan(u64),
  /// `len >= threshold`.
  AtLeast(u64),
}

impl SizePredicate {
  /// True when `len` satisfies the comparison.
  pub fn matches(&self, len: u64) -> bool {
    match *self {
      SizePredicate::GreaterThan(t) => len > t,
      SizePredicate::AtLeast(t) => len >= t,
    }
  }

  /// Parse `"> 1MB"` / `">=1024"` / `"> 4 KiB"` into a predicate. Whitespace is
  /// optional everywhere; the operator must be `>` or `>=`; the unit (if any) is
  /// case-insensitive.
  pub fn parse(spec: &str) -> Result<SizePredicate, GateParseError> {
    let trimmed = spec.trim();
    let (at_least, rest) = if let Some(rest) = trimmed.strip_prefix(">=") {
      (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix('>') {
      (false, rest)
    } else {
      return Err(GateParseError::Operator);
    };
    let bytes = parse_size(rest.trim())?;
    Ok(if at_least {
      SizePredicate::AtLeast(bytes)
    } else {
      SizePredicate::GreaterThan(bytes)
    })
  }
}

/// Parse a size literal (`"1MB"`, `"4 KiB"`, `"512"`) into a byte count. A bare
/// number is bytes; a unit suffix scales it. Decimal units are powers of 1000,
/// binary units (the `i` forms) powers of 1024.
fn parse_size(spec: &str) -> Result<u64, GateParseError> {
  let spec = spec.trim();
  if spec.is_empty() {
    return Err(GateParseError::Number);
  }
  // Split the leading digit run from the trailing unit.
  let split = spec
    .find(|c: char| !c.is_ascii_digit() && c != '_')
    .unwrap_or(spec.len());
  let (digits, unit) = spec.split_at(split);
  let number: u64 = digits
    .replace('_', "")
    .parse()
    .map_err(|_| GateParseError::Number)?;
  let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
    "" | "b" => 1,
    "kb" => 1_000,
    "mb" => 1_000_000,
    "gb" => 1_000_000_000,
    "kib" => 1024,
    "mib" => 1024 * 1024,
    "gib" => 1024 * 1024 * 1024,
    _ => return Err(GateParseError::Unit),
  };
  number
    .checked_mul(multiplier)
    .ok_or(GateParseError::Overflow)
}

/// Why a `Gate` / `SizePredicate` string failed to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateParseError {
  /// The size predicate did not start with `>` or `>=`.
  Operator,
  /// The numeric portion was missing or not an integer.
  Number,
  /// The unit suffix was not one of B/KB/MB/GB/KiB/MiB/GiB.
  Unit,
  /// The number × unit overflowed u64.
  Overflow,
}

impl std::fmt::Display for GateParseError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let msg = match self {
      GateParseError::Operator => "size predicate must start with '>' or '>='",
      GateParseError::Number => "size predicate needs an integer (e.g. '> 1MB')",
      GateParseError::Unit => "unknown size unit (use B/KB/MB/GB/KiB/MiB/GiB)",
      GateParseError::Overflow => "size predicate overflows a 64-bit byte count",
    };
    f.write_str(msg)
  }
}

impl std::error::Error for GateParseError {}

/// The install-time gate: an optional glob AND an optional size predicate. A file
/// matches only if BOTH present predicates pass (an absent half is vacuously true).
/// `Gate::default()` is the fleet default — glob `**/*.node`, no size floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
  glob: Option<String>,
  size: Option<SizePredicate>,
}

impl Default for Gate {
  fn default() -> Self {
    Gate {
      glob: Some(DEFAULT_GLOB.to_string()),
      size: None,
    }
  }
}

/// The fleet default — every native addon, regardless of size.
pub const DEFAULT_GLOB: &str = "**/*.node";

impl Gate {
  /// A gate that matches everything (no glob, no size floor). Useful when the
  /// caller has already selected the file and just wants the one-pass writer.
  pub fn any() -> Self {
    Gate {
      glob: None,
      size: None,
    }
  }

  /// Build a gate from an optional glob and an optional size-predicate string. A
  /// `None` glob matches any name; a `None`/empty size string applies no floor.
  pub fn new(glob: Option<&str>, size: Option<&str>) -> Result<Gate, GateParseError> {
    let size = match size {
      None => None,
      Some(s) if s.trim().is_empty() => None,
      Some(s) => Some(SizePredicate::parse(s)?),
    };
    Ok(Gate {
      glob: glob.map(str::to_string),
      size,
    })
  }

  /// Replace the glob (chainable builder).
  pub fn with_glob(mut self, glob: &str) -> Self {
    self.glob = Some(glob.to_string());
    self
  }

  /// Replace the size predicate (chainable builder).
  pub fn with_size(mut self, size: SizePredicate) -> Self {
    self.size = Some(size);
    self
  }

  /// The glob pattern, if any.
  pub fn glob(&self) -> Option<&str> {
    self.glob.as_deref()
  }

  /// The size predicate, if any.
  pub fn size(&self) -> Option<SizePredicate> {
    self.size
  }

  /// True when `name` matches the glob (if set) AND `len` satisfies the size
  /// predicate (if set). The name is matched against the full path the caller
  /// passes — pass a `/`-normalized path so `**` segments line up.
  pub fn matches(&self, name: &str, len: u64) -> bool {
    if let Some(glob) = &self.glob {
      if !glob_match(glob, name) {
        return false;
      }
    }
    if let Some(size) = &self.size {
      if !size.matches(len) {
        return false;
      }
    }
    true
  }
}

/// A small glob matcher covering the subset the gate needs: `*` (any run within a
/// path segment, no `/`), `**` (any run across segments, including `/`), and `?`
/// (one non-`/` char). Literal bytes match themselves. No char classes — the gate
/// patterns are simple suffix/segment globs like `**/*.node`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
  glob_inner(pattern.as_bytes(), text.as_bytes())
}

/// Recursive matcher — clean handling of nested stars (a `*` inside a `**` tail,
/// like `**/*.node`) that a single backtrack slot can't express. The pattern set
/// is small (suffix/segment globs), so the recursion depth is the number of stars,
/// not the input length.
fn glob_inner(pat: &[u8], text: &[u8]) -> bool {
  // Pattern exhausted: match iff text is too.
  let Some(&pc) = pat.first() else {
    return text.is_empty();
  };
  match pc {
    b'*' => {
      let double = pat.get(1) == Some(&b'*');
      if double {
        // `**` matches any run including `/`. If immediately followed by `/`, that
        // slash may also collapse to nothing (a/**/b ⊇ a/b).
        let after = &pat[2..];
        let collapsed = after.strip_prefix(b"/").unwrap_or(after);
        // Zero-width match (with the optional `/` collapse), then every longer run.
        if glob_inner(collapsed, text) || glob_inner(after, text) {
          return true;
        }
        for i in 0..text.len() {
          if glob_inner(after, &text[i + 1..]) || glob_inner(collapsed, &text[i + 1..]) {
            return true;
          }
        }
        false
      } else {
        // `*` matches a run within one path segment — never a `/`.
        let rest = &pat[1..];
        if glob_inner(rest, text) {
          return true;
        }
        for i in 0..text.len() {
          if text[i] == b'/' {
            break;
          }
          if glob_inner(rest, &text[i + 1..]) {
            return true;
          }
        }
        false
      }
    }
    b'?' => match text.first() {
      Some(&c) if c != b'/' => glob_inner(&pat[1..], &text[1..]),
      _ => false,
    },
    c => match text.first() {
      Some(&t) if t == c => glob_inner(&pat[1..], &text[1..]),
      _ => false,
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_units_case_insensitively() {
    assert_eq!(parse_size("512"), Ok(512));
    assert_eq!(parse_size("512B"), Ok(512));
    assert_eq!(parse_size("1kb"), Ok(1_000));
    assert_eq!(parse_size("1KB"), Ok(1_000));
    assert_eq!(parse_size("2MB"), Ok(2_000_000));
    assert_eq!(parse_size("3gb"), Ok(3_000_000_000));
    assert_eq!(parse_size("1KiB"), Ok(1024));
    assert_eq!(parse_size("1mib"), Ok(1024 * 1024));
    assert_eq!(parse_size("1GiB"), Ok(1024 * 1024 * 1024));
    // Whitespace + digit separators tolerated.
    assert_eq!(parse_size("1 MB"), Ok(1_000_000));
    assert_eq!(parse_size("1_000"), Ok(1_000));
  }

  #[test]
  fn rejects_bad_size_literals() {
    assert_eq!(parse_size(""), Err(GateParseError::Number));
    assert_eq!(parse_size("MB"), Err(GateParseError::Number));
    assert_eq!(parse_size("10PB"), Err(GateParseError::Unit));
    assert_eq!(parse_size("99999999999999999999GB"), Err(GateParseError::Number));
    assert_eq!(
      parse_size("18446744073709551615KB"),
      Err(GateParseError::Overflow)
    );
  }

  #[test]
  fn parses_predicate_operators() {
    assert_eq!(
      SizePredicate::parse("> 1MB"),
      Ok(SizePredicate::GreaterThan(1_000_000))
    );
    assert_eq!(
      SizePredicate::parse(">=1MB"),
      Ok(SizePredicate::AtLeast(1_000_000))
    );
    assert_eq!(
      SizePredicate::parse("  >=  4 KiB "),
      Ok(SizePredicate::AtLeast(4096))
    );
    assert_eq!(SizePredicate::parse("1MB"), Err(GateParseError::Operator));
    assert_eq!(SizePredicate::parse("< 1MB"), Err(GateParseError::Operator));
  }

  #[test]
  fn predicate_comparison_is_exact_at_the_boundary() {
    let gt = SizePredicate::GreaterThan(1000);
    assert!(!gt.matches(1000));
    assert!(gt.matches(1001));
    let ge = SizePredicate::AtLeast(1000);
    assert!(ge.matches(1000));
    assert!(!ge.matches(999));
  }

  #[test]
  fn glob_matches_node_addons_anywhere() {
    assert!(glob_match("**/*.node", "node_modules/foo/build/Release/addon.node"));
    assert!(glob_match("**/*.node", "addon.node"));
    assert!(glob_match("*.node", "addon.node"));
    // A single * must not cross a path separator.
    assert!(!glob_match("*.node", "dir/addon.node"));
    assert!(!glob_match("**/*.node", "addon.so"));
  }

  #[test]
  fn glob_handles_question_and_literal_and_double_star_edges() {
    assert!(glob_match("a?c", "abc"));
    assert!(!glob_match("a?c", "a/c"));
    assert!(glob_match("**", "any/deep/path"));
    assert!(glob_match("a/**/b", "a/x/y/b"));
    assert!(glob_match("a/**/b", "a/b"));
    assert!(glob_match("exact", "exact"));
    assert!(!glob_match("exact", "exacted"));
    // Trailing star eats the rest.
    assert!(glob_match("pre*", "prefix"));
  }

  #[test]
  fn gate_default_is_node_glob_no_floor() {
    let g = Gate::default();
    assert_eq!(g.glob(), Some("**/*.node"));
    assert_eq!(g.size(), None);
    assert!(g.matches("build/Release/x.node", 10));
    assert!(!g.matches("build/Release/x.so", 10));
  }

  #[test]
  fn gate_requires_both_halves() {
    let g = Gate::new(Some("**/*.node"), Some(">= 1MB")).unwrap();
    assert!(g.matches("a/b.node", 2_000_000));
    // Right name, too small.
    assert!(!g.matches("a/b.node", 500_000));
    // Big enough, wrong name.
    assert!(!g.matches("a/b.so", 2_000_000));
  }

  #[test]
  fn gate_any_matches_everything() {
    let g = Gate::any();
    assert!(g.matches("whatever.xyz", 0));
    assert_eq!(g.glob(), None);
    assert_eq!(g.size(), None);
  }

  #[test]
  fn gate_new_treats_empty_size_as_no_floor() {
    let g = Gate::new(None, Some("   ")).unwrap();
    assert_eq!(g.size(), None);
    assert!(g.matches("anything", 0));
  }

  #[test]
  fn gate_builders_chain() {
    let g = Gate::any()
      .with_glob("**/*.dylib")
      .with_size(SizePredicate::GreaterThan(100));
    assert!(g.matches("a/b.dylib", 200));
    assert!(!g.matches("a/b.dylib", 50));
  }

  #[test]
  fn gate_new_propagates_parse_errors() {
    assert_eq!(Gate::new(None, Some("nope")), Err(GateParseError::Operator));
  }

  #[test]
  fn gate_new_with_a_glob_and_no_size_applies_no_floor() {
    // The `None` size arm of Gate::new — a glob with no size predicate.
    let g = Gate::new(Some("**/*.node"), None).unwrap();
    assert_eq!(g.size(), None);
    assert!(g.matches("a/b.node", 1));
    assert!(!g.matches("a/b.so", 1));
  }

  #[test]
  fn single_star_never_spans_a_separator() {
    // A single `*` matches a run within one segment and must stop at a `/`.
    let g = Gate::new(Some("a*c"), None).unwrap();
    assert!(g.matches("ac", 0), "the star matches a zero-width run");
    assert!(g.matches("abc", 0), "matches within one segment");
    assert!(!g.matches("ab/c", 0), "a single * never crosses a /");
  }

  #[test]
  fn parse_error_display_is_distinct() {
    let msgs: Vec<String> = [
      GateParseError::Operator,
      GateParseError::Number,
      GateParseError::Unit,
      GateParseError::Overflow,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    // All four messages are non-empty and unique.
    assert!(msgs.iter().all(|m| !m.is_empty()));
    let mut sorted = msgs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 4);
  }
}
