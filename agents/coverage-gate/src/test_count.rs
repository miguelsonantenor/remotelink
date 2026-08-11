//! Count unit/integration tests in a package tree (presence gate).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Walk `root` for `.rs` files (skips `target/` and hidden dirs).
pub fn walk_rust_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_rust_files_inner(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_rust_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if path.is_dir() {
            walk_rust_files_inner(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Count test attributes in a single source string.
///
/// Matches line-oriented attributes used by this repo and common async/test
/// crates: `#[test]`, `#[tokio::test…]`, `#[async_std::test…]`, `#[rstest…]`.
pub fn count_tests_in_source(source: &str) -> u32 {
    let mut n = 0u32;
    for line in source.lines() {
        let t = line.trim();
        if t.starts_with("#[test]")
            || t.starts_with("#[tokio::test")
            || t.starts_with("#[async_std::test")
            || t.starts_with("#[rstest")
        {
            n = n.saturating_add(1);
        }
    }
    n
}

/// Count tests under a package root (`src/` + `tests/` + root-level `*.rs`).
pub fn count_package_tests(package_root: &Path) -> io::Result<u32> {
    let mut total = 0u32;
    for sub in ["src", "tests"] {
        let dir = package_root.join(sub);
        if !dir.is_dir() {
            continue;
        }
        for file in walk_rust_files(&dir)? {
            let text = fs::read_to_string(&file)?;
            total = total.saturating_add(count_tests_in_source(&text));
        }
    }
    // Root-level `lib.rs` / `main.rs` (no `src/` layout). Do not re-walk
    // `tests/` — already counted above.
    if !package_root.join("src").is_dir() {
        if let Ok(entries) = fs::read_dir(package_root) {
            for entry in entries {
                let path = entry?.path();
                if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    let text = fs::read_to_string(&path)?;
                    total = total.saturating_add(count_tests_in_source(&text));
                }
            }
        }
    }
    Ok(total)
}

/// True if the package has a `tests/` tree with `.rs` files or any test attr.
pub fn package_has_test_surface(package_root: &Path) -> io::Result<bool> {
    Ok(count_package_tests(package_root)? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn counts_plain_and_tokio() {
        let src = r#"
#[test]
fn a() {}

#[tokio::test]
async fn b() {}

#[tokio::test(flavor = "multi_thread")]
async fn c() {}

// not a test: #[test] in comment is still counted if alone — use real lines
fn not_test() {}
"#;
        // Comment line with `// not a test: #[test]` does NOT start with #[test]
        assert_eq!(count_tests_in_source(src), 3);
    }

    #[test]
    fn ignores_indented_non_attrs() {
        assert_eq!(count_tests_in_source("    let s = \"#[test]\";\n"), 0);
    }

    #[test]
    fn count_package_on_temp_tree() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let mut f = fs::File::create(src.join("lib.rs")).unwrap();
        writeln!(
            f,
            "pub fn x() {{}}\n#[cfg(test)]\nmod t {{\n#[test]\nfn one() {{}}\n#[test]\nfn two() {{}}\n}}\n"
        )
        .unwrap();
        assert_eq!(count_package_tests(dir.path()).unwrap(), 2);
        assert!(package_has_test_surface(dir.path()).unwrap());
    }

    #[test]
    fn empty_package_zero() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("lib.rs"), "pub fn x() {}\n").unwrap();
        assert_eq!(count_package_tests(dir.path()).unwrap(), 0);
        assert!(!package_has_test_surface(dir.path()).unwrap());
    }

    #[test]
    fn counts_integration_tests_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tests = dir.path().join("tests");
        fs::create_dir_all(&tests).unwrap();
        fs::write(tests.join("it.rs"), "#[test]\nfn integ() {}\n").unwrap();
        assert_eq!(count_package_tests(dir.path()).unwrap(), 1);
    }
}
