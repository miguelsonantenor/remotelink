//! Source walk + public-item parser for workspace crates.

use crate::types::{Inventory, InventoryItem, ItemKind, PackageSummary, Visibility};
use serde_json::Value;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Errors from inventory collection.
#[derive(Debug)]
pub enum InventoryError {
    /// I/O failure.
    Io(io::Error),
    /// `cargo metadata` failed or returned invalid JSON.
    Metadata(String),
    /// JSON parse error.
    Json(serde_json::Error),
}

impl std::fmt::Display for InventoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InventoryError::Io(e) => write!(f, "io error: {e}"),
            InventoryError::Metadata(e) => write!(f, "cargo metadata: {e}"),
            InventoryError::Json(e) => write!(f, "json error: {e}"),
        }
    }
}

impl std::error::Error for InventoryError {}

impl From<io::Error> for InventoryError {
    fn from(e: io::Error) -> Self {
        InventoryError::Io(e)
    }
}

impl From<serde_json::Error> for InventoryError {
    fn from(e: serde_json::Error) -> Self {
        InventoryError::Json(e)
    }
}

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
    let entries = fs::read_dir(dir)?;
    for entry in entries {
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

/// True if the package tree looks like it has unit or integration tests.
///
/// Heuristics:
/// - any `tests/` directory with `.rs` files under the package root
/// - any `#[test]` attribute in package `.rs` sources
pub fn package_has_tests(package_root: &Path) -> io::Result<bool> {
    let tests_dir = package_root.join("tests");
    if tests_dir.is_dir() {
        let files = walk_rust_files(&tests_dir)?;
        if !files.is_empty() {
            return Ok(true);
        }
    }
    let src = package_root.join("src");
    let roots = if src.is_dir() {
        vec![src]
    } else {
        vec![package_root.to_path_buf()]
    };
    for root in roots {
        for file in walk_rust_files(&root)? {
            let text = fs::read_to_string(&file)?;
            if source_has_test_attr(&text) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Detect `#[test]` / `#[tokio::test]` style attributes (line-oriented).
pub fn source_has_test_attr(source: &str) -> bool {
    for line in source.lines() {
        let t = line.trim();
        if t.starts_with("#[test]")
            || t.starts_with("#[tokio::test")
            || t.starts_with("#[async_std::test")
            || t.starts_with("#[rstest")
        {
            return true;
        }
    }
    false
}

/// Parse public items from a single Rust source string.
///
/// Line-oriented scanner: finds `pub` / `pub(...)` item declarations at the
/// start of a (possibly attribute-decorated) item. Skips content inside
/// `#[cfg(test)]` modules. Brace depth tracks across multi-line (raw) strings
/// so test fixtures containing sample `pub` source are not inventoried.
pub fn parse_public_items(source: &str, crate_name: &str, rel_path: &str) -> Vec<InventoryItem> {
    let mut items = Vec::new();
    // When > 0, we are inside a skipped #[cfg(test)] module body.
    let mut skip_brace_depth: i32 = 0;
    let mut pending_cfg_test = false;
    // (brace depth when the impl was entered, type name)
    let mut impl_stack: Vec<(i32, String)> = Vec::new();
    // Global brace depth outside strings (for impl pop).
    let mut file_brace_depth: i32 = 0;
    let mut str_state = StringState::Code;

    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line_no = (i + 1) as u32;
        let raw = lines[i];
        // If mid-string from previous line, do not strip comments / parse as code.
        let (code_line, net_braces, new_state) = scan_line_code(raw, str_state);
        str_state = new_state;

        if skip_brace_depth > 0 {
            skip_brace_depth += net_braces;
            if skip_brace_depth <= 0 {
                skip_brace_depth = 0;
            }
            file_brace_depth += net_braces;
            while impl_stack
                .last()
                .is_some_and(|(entry, _)| file_brace_depth <= *entry)
            {
                impl_stack.pop();
            }
            i += 1;
            continue;
        }

        let trimmed = code_line.trim();
        if trimmed.is_empty() && matches!(str_state, StringState::Code) {
            file_brace_depth += net_braces;
            i += 1;
            continue;
        }

        // Only interpret attributes / items when fully in code on this line.
        if matches!(str_state, StringState::Code) || !trimmed.is_empty() {
            if is_cfg_test_attr(trimmed) {
                pending_cfg_test = true;
                file_brace_depth += net_braces;
                i += 1;
                continue;
            }
            if trimmed.starts_with("#[") {
                file_brace_depth += net_braces;
                i += 1;
                continue;
            }
        }

        if pending_cfg_test && matches!(str_state, StringState::Code) {
            if trimmed.starts_with("mod ") {
                if trimmed.contains('{') {
                    // Body starts this line; skip until braces balance.
                    skip_brace_depth = net_braces.max(1);
                    pending_cfg_test = false;
                    file_brace_depth += net_braces;
                    i += 1;
                    continue;
                }
                if trimmed.ends_with(';') {
                    // `#[cfg(test)] mod foo;` — nothing to skip further.
                    pending_cfg_test = false;
                    file_brace_depth += net_braces;
                    i += 1;
                    continue;
                }
                // `#[cfg(test)] mod foo` then `{` on next line.
                file_brace_depth += net_braces;
                i += 1;
                continue;
            }
            if trimmed == "{" {
                skip_brace_depth = 1;
                pending_cfg_test = false;
                file_brace_depth += 1;
                i += 1;
                continue;
            }
            // Non-mod cfg(test) item: skip this line only.
            pending_cfg_test = false;
            file_brace_depth += net_braces;
            i += 1;
            continue;
        }

        // Track impl blocks for method qualification (code only).
        if matches!(str_state, StringState::Code) || !code_line.trim().is_empty() {
            let trimmed = code_line.trim();
            if let Some(ty) = parse_impl_type(trimmed) {
                impl_stack.push((file_brace_depth, ty));
            }

            // Multi-line `pub use ...;` — accumulate until `;` at brace depth 0.
            if is_pub_use_start(trimmed) {
                let start_line = line_no;
                let mut use_stmt = trimmed.to_string();
                let mut end = i;
                let mut acc_str_state = str_state;
                let mut acc_depth = file_brace_depth + net_braces;

                while !statement_complete_at_semi(&use_stmt) && end + 1 < lines.len() {
                    end += 1;
                    let (more, more_net, st) = scan_line_code(lines[end], acc_str_state);
                    acc_str_state = st;
                    acc_depth += more_net;
                    if !more.trim().is_empty() {
                        use_stmt.push(' ');
                        use_stmt.push_str(more.trim());
                    }
                }

                if let Some((vis, names)) = parse_pub_use_statement(&use_stmt) {
                    for name in names {
                        items.push(InventoryItem {
                            crate_name: crate_name.to_string(),
                            path: rel_path.replace('\\', "/"),
                            item: name,
                            kind: ItemKind::Use,
                            visibility: vis.clone(),
                            line: Some(start_line),
                        });
                    }
                }

                // Apply brace depth for every consumed line; advance past them.
                file_brace_depth = acc_depth;
                str_state = acc_str_state;
                while impl_stack
                    .last()
                    .is_some_and(|(entry, _)| file_brace_depth <= *entry)
                {
                    impl_stack.pop();
                }
                i = end + 1;
                continue;
            }

            if let Some((vis, kind, name)) = parse_pub_item_line(trimmed) {
                // Non-use items only here (`use` handled above).
                if kind != ItemKind::Use {
                    let item_name = qualify_item_name_simple(&impl_stack, kind, &name);
                    items.push(InventoryItem {
                        crate_name: crate_name.to_string(),
                        path: rel_path.replace('\\', "/"),
                        item: item_name,
                        kind,
                        visibility: vis,
                        line: Some(line_no),
                    });
                } else {
                    // Single-line use that somehow missed is_pub_use_start.
                    if let Some((vis, names)) = parse_pub_use_statement(trimmed) {
                        for name in names {
                            items.push(InventoryItem {
                                crate_name: crate_name.to_string(),
                                path: rel_path.replace('\\', "/"),
                                item: name,
                                kind: ItemKind::Use,
                                visibility: vis.clone(),
                                line: Some(line_no),
                            });
                        }
                    }
                }
            }
        }

        file_brace_depth += net_braces;
        while impl_stack
            .last()
            .is_some_and(|(entry, _)| file_brace_depth <= *entry)
        {
            impl_stack.pop();
        }
        i += 1;
    }

    items
}

/// True for `#[cfg(test)]` and space-tolerant `#[cfg(all(test, ...))]`.
/// Does **not** match `#[cfg(not(test))]`.
pub fn is_cfg_test_attr(trimmed: &str) -> bool {
    let t = trimmed.trim();
    if !t.starts_with("#[cfg(") {
        return false;
    }
    // Drop whitespace so `#[cfg( all( test , ...` matches.
    let compact: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.starts_with("#[cfg(test)]") {
        return true;
    }
    if let Some(inner) = compact.strip_prefix("#[cfg(all(") {
        let inner = inner
            .strip_suffix(")]")
            .or_else(|| inner.strip_suffix(']'))
            .unwrap_or(inner);
        return cfg_all_args(inner).iter().any(|a| a == "test");
    }
    false
}

fn cfg_all_args(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                let t = cur.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

fn is_pub_use_start(trimmed: &str) -> bool {
    let t = trimmed.trim_start();
    if let Some(rest) = t.strip_prefix("pub") {
        let rest = if rest.starts_with('(') {
            match rest.find(')') {
                Some(i) => rest[i + 1..].trim_start(),
                None => return false,
            }
        } else {
            rest.trim_start()
        };
        return rest.starts_with("use ") || rest == "use" || rest.starts_with("use\t");
    }
    false
}

/// Statement ends when a `;` appears at brace depth 0 (outside of simple nesting).
fn statement_complete_at_semi(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ';' if depth <= 0 => return true,
            _ => {}
        }
    }
    false
}

/// Parse a full `pub use ...;` statement into visibility + expanded import names.
///
/// Brace groups expand to one item per imported name (`path::Name`).
pub fn parse_pub_use_statement(stmt: &str) -> Option<(Visibility, Vec<String>)> {
    let t = stmt.trim();
    if !is_pub_use_start(t) {
        return None;
    }
    let (vis, after_vis) = split_vis(t)?;
    let rest = after_vis.trim_start();
    let body = rest.strip_prefix("use")?.trim_start();
    let names = expand_use_imports(body);
    if names.is_empty() {
        None
    } else {
        Some((vis, names))
    }
}

fn split_vis(t: &str) -> Option<(Visibility, &str)> {
    let t = t.trim_start();
    if t.starts_with("pub(") {
        let close = t.find(')')?;
        let vis = Visibility::parse(&t[..=close]);
        Some((vis, &t[close + 1..]))
    } else if t.starts_with("pub ") || t.starts_with("pub\t") {
        Some((Visibility::Pub, &t[3..]))
    } else if t.starts_with("pub") {
        // `pubuse` invalid
        None
    } else {
        None
    }
}

/// Expand `use` tree text (after the `use` keyword) into concrete item paths.
pub fn expand_use_imports(body: &str) -> Vec<String> {
    let s = body
        .split(';')
        .next()
        .unwrap_or(body)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }

    if let Some(brace_at) = s.find('{') {
        let prefix = s[..brace_at].trim();
        // prefix is typically `path::` or `path::path::`
        let prefix = if prefix.is_empty() {
            String::new()
        } else if prefix.ends_with("::") {
            prefix.to_string()
        } else {
            format!("{prefix}::")
        };
        let after = &s[brace_at + 1..];
        let end = match after.rfind('}') {
            Some(i) => i,
            None => after.len(),
        };
        let inner = after[..end].trim();
        let mut out = Vec::new();
        for part in split_comma_top_level(inner) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            // Nested braces: `foo::{Bar, Baz}` inside — rare; keep as nested text.
            if part.contains('{') {
                out.push(format!("{prefix}{part}"));
                continue;
            }
            // `name as Alias` → record both forms as `name as Alias` under prefix.
            out.push(format!("{prefix}{part}"));
        }
        if out.is_empty() {
            // Empty group `path::{}` — keep the group form.
            out.push(format!("{prefix}{{}}"));
        }
        out
    } else {
        vec![s.to_string()]
    }
}

fn split_comma_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringState {
    Code,
    NormalStr,
    RawStr { hashes: usize },
}

/// Scan one source line starting in `state`; return (code-only text, net braces, new state).
fn scan_line_code(line: &str, mut state: StringState) -> (String, i32, StringState) {
    let mut code = String::new();
    let mut net = 0i32;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        match state {
            StringState::Code => {
                // Line comment
                if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
                    break;
                }
                // Raw string: r##"..."##
                if chars[i] == 'r' {
                    let mut j = i + 1;
                    let mut hashes = 0usize;
                    while j < chars.len() && chars[j] == '#' {
                        hashes += 1;
                        j += 1;
                    }
                    if j < chars.len() && chars[j] == '"' {
                        state = StringState::RawStr { hashes };
                        i = j + 1;
                        continue;
                    }
                }
                // Normal string
                if chars[i] == '"' {
                    state = StringState::NormalStr;
                    i += 1;
                    continue;
                }
                if chars[i] == '{' {
                    net += 1;
                } else if chars[i] == '}' {
                    net -= 1;
                }
                code.push(chars[i]);
                i += 1;
            }
            StringState::NormalStr => {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if chars[i] == '"' {
                    state = StringState::Code;
                }
                i += 1;
            }
            StringState::RawStr { hashes } => {
                if chars[i] == '"' {
                    let mut ok = true;
                    for h in 0..hashes {
                        if i + 1 + h >= chars.len() || chars[i + 1 + h] != '#' {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        i += 1 + hashes;
                        state = StringState::Code;
                        continue;
                    }
                }
                i += 1;
            }
        }
    }

    (code, net, state)
}

fn qualify_item_name_simple(impl_stack: &[(i32, String)], kind: ItemKind, name: &str) -> String {
    if kind == ItemKind::Fn {
        if let Some((_, ty)) = impl_stack.last() {
            return format!("{ty}::{name}");
        }
    }
    name.to_string()
}

fn parse_impl_type(trimmed: &str) -> Option<String> {
    let t = trimmed.trim_start();
    // `impl Foo {` / `impl Foo for Bar {` / `impl<'a> Foo`
    let rest = t.strip_prefix("impl")?;
    // Require a delimiter so we do not match identifiers like `implements`.
    let first = rest.chars().next()?;
    if !(first.is_whitespace() || first == '<' || first == '!') {
        return None;
    }
    if rest.trim_start().starts_with('!') {
        // Negative impls — ignore for method qualification.
        return None;
    }
    let rest = rest.trim_start();
    // Skip generics: impl<...> Type
    let rest = if rest.starts_with('<') {
        skip_angle(rest)?.trim_start()
    } else {
        rest
    };
    if rest.is_empty() {
        return None;
    }
    // Trait impl: Type after `for`
    if let Some(idx) = rest.find(" for ") {
        let after = rest[idx + 5..].trim();
        return Some(ident_head(after)?.to_string());
    }
    Some(ident_head(rest)?.to_string())
}

fn skip_angle(s: &str) -> Option<&str> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[i + 1..]);
                }
            }
            _ => {}
        }
    }
    None
}

fn ident_head(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let end = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphanumeric() && *c != '_')
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end == 0 {
        None
    } else {
        Some(&s[..end])
    }
}

/// Parse a single logical line that may declare a public item.
///
/// Returns `(visibility, kind, name)`.
pub fn parse_pub_item_line(trimmed: &str) -> Option<(Visibility, ItemKind, String)> {
    let t = trimmed.trim_start();
    if !t.starts_with("pub") {
        return None;
    }
    // Must be `pub` or `pub(...)` not `public` or `pubic`.
    let after_pub = if t.starts_with("pub(") {
        let close = t.find(')')?;
        &t[close + 1..]
    } else if t.starts_with("pub ") || t.starts_with("pub\t") {
        &t[3..]
    } else {
        return None;
    };

    let vis_str = if t.starts_with("pub(") {
        let close = t.find(')')?;
        &t[..=close]
    } else {
        "pub"
    };
    let vis = Visibility::parse(vis_str);

    let rest = after_pub.trim_start();
    // Skip async / unsafe / const / extern prefixes for fns.
    let rest = skip_fn_prefixes(rest);

    let (kind, name_src) = if let Some(r) = rest.strip_prefix("fn ") {
        (ItemKind::Fn, r)
    } else if let Some(r) = rest.strip_prefix("struct ") {
        (ItemKind::Struct, r)
    } else if let Some(r) = rest.strip_prefix("enum ") {
        (ItemKind::Enum, r)
    } else if let Some(r) = rest.strip_prefix("trait ") {
        (ItemKind::Trait, r)
    } else if let Some(r) = rest.strip_prefix("type ") {
        (ItemKind::Type, r)
    } else if let Some(r) = rest.strip_prefix("const ") {
        (ItemKind::Const, r)
    } else if let Some(r) = rest.strip_prefix("static ") {
        (ItemKind::Static, r)
    } else if let Some(r) = rest.strip_prefix("mod ") {
        (ItemKind::Mod, r)
    } else if let Some(r) = rest.strip_prefix("use ") {
        (ItemKind::Use, r)
    } else if let Some(r) = rest.strip_prefix("macro_rules! ") {
        (ItemKind::Macro, r)
    } else {
        let r = rest.strip_prefix("macro ")?;
        (ItemKind::Macro, r)
    };

    let name = match kind {
        ItemKind::Use => normalize_use_name(name_src),
        _ => ident_head(name_src)?.to_string(),
    };

    Some((vis, kind, name))
}

fn skip_fn_prefixes(s: &str) -> &str {
    let mut s = s;
    loop {
        let t = s.trim_start();
        if let Some(r) = t.strip_prefix("async ") {
            s = r;
            continue;
        }
        if let Some(r) = t.strip_prefix("const ") {
            // const fn vs const ITEM — if next is `fn`, keep stripping.
            if r.trim_start().starts_with("fn ") || r.trim_start().starts_with("async ") {
                s = r;
                continue;
            }
            // else leave for const item handling
            return t;
        }
        if let Some(r) = t.strip_prefix("unsafe ") {
            s = r;
            continue;
        }
        if let Some(r) = t.strip_prefix("extern ") {
            // extern "C" fn …
            let r = r.trim_start();
            if let Some(stripped) = r.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    s = stripped[end + 1..].trim_start();
                    continue;
                }
            }
            s = r;
            continue;
        }
        return t;
    }
}

fn normalize_use_name(src: &str) -> String {
    // Take up to `;` and compress whitespace.
    let s = src.split(';').next().unwrap_or(src).trim();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Run inventory over a Cargo workspace root.
pub fn inventory_workspace(workspace_root: &Path) -> Result<Inventory, InventoryError> {
    // Canonicalize for reliable relative paths; display as "." when scanning CWD.
    let abs_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let display_root = {
        let n = normalize_path_display(&abs_root);
        let cwd = env::current_dir()
            .ok()
            .and_then(|c| c.canonicalize().ok())
            .map(|c| normalize_path_display(&c));
        if cwd.as_ref() == Some(&n) {
            ".".to_string()
        } else {
            n
        }
    };

    let meta = cargo_metadata(&abs_root)?;
    let packages = workspace_packages(&meta, &abs_root)?;

    let mut all_items = Vec::new();
    let mut summaries = Vec::new();

    for pkg in packages {
        let is_lib = pkg.is_lib;
        let has_tests = package_has_tests(&pkg.root_dir).unwrap_or(false);
        let mut public_count = 0usize;

        if is_lib {
            let src_root = pkg
                .lib_src
                .as_ref()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| pkg.root_dir.join("src"));

            if src_root.is_dir() {
                for file in walk_rust_files(&src_root)? {
                    let rel = path_relative(&abs_root, &file);
                    let text = fs::read_to_string(&file)?;
                    let parsed = parse_public_items(&text, &pkg.name, &rel);
                    for item in &parsed {
                        if item.visibility.is_public_api() {
                            public_count += 1;
                        }
                    }
                    all_items.extend(parsed);
                }
            }
        }

        summaries.push(PackageSummary {
            name: pkg.name,
            manifest_path: path_relative(&abs_root, &pkg.manifest_path),
            is_lib,
            has_tests,
            public_item_count: public_count,
        });
    }

    all_items
        .sort_by(|a, b| (&a.crate_name, &a.path, &a.item).cmp(&(&b.crate_name, &b.path, &b.item)));
    summaries.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Inventory {
        schema_version: 1,
        workspace_root: display_root,
        generated_at: now_stamp(),
        items: all_items,
        packages: summaries,
    })
}

/// Display path with forward slashes; strip Windows `\\?\` extended prefix.
fn normalize_path_display(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    s
}

struct WorkspacePackage {
    name: String,
    root_dir: PathBuf,
    manifest_path: PathBuf,
    is_lib: bool,
    lib_src: Option<PathBuf>,
}

fn cargo_metadata(workspace_root: &Path) -> Result<Value, InventoryError> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root)
        .output()
        .map_err(|e| InventoryError::Metadata(format!("failed to spawn cargo: {e}")))?;

    if !output.status.success() {
        return Err(InventoryError::Metadata(format!(
            "exit {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let v: Value = serde_json::from_slice(&output.stdout)?;
    Ok(v)
}

fn workspace_packages(
    meta: &Value,
    workspace_root: &Path,
) -> Result<Vec<WorkspacePackage>, InventoryError> {
    let members = meta
        .get("workspace_members")
        .and_then(|v| v.as_array())
        .ok_or_else(|| InventoryError::Metadata("missing workspace_members".into()))?;

    let packages = meta
        .get("packages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| InventoryError::Metadata("missing packages".into()))?;

    let member_ids: std::collections::HashSet<&str> =
        members.iter().filter_map(|v| v.as_str()).collect();

    let mut out = Vec::new();
    for pkg in packages {
        let id = pkg.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !member_ids.contains(id) {
            // Fallback match by name for older cargo path ids.
            let name = pkg.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let matched = member_ids.iter().any(|m| m.contains(name));
            if !matched {
                continue;
            }
        }

        let name = pkg
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| InventoryError::Metadata("package without name".into()))?
            .to_string();

        // Skip the agent itself from "product" inventory? Include it — useful.
        let manifest_path = pkg
            .get("manifest_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| InventoryError::Metadata("package without manifest_path".into()))?;
        let manifest_path = PathBuf::from(manifest_path);
        let root_dir = manifest_path
            .parent()
            .unwrap_or(workspace_root)
            .to_path_buf();

        let targets = pkg
            .get("targets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut is_lib = false;
        let mut lib_src = None;
        for t in &targets {
            let kinds = t
                .get("kind")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let is_lib_target = kinds.iter().any(|k| k.as_str() == Some("lib"));
            if is_lib_target {
                is_lib = true;
                if let Some(src) = t.get("src_path").and_then(|v| v.as_str()) {
                    lib_src = Some(PathBuf::from(src));
                }
            }
        }

        out.push(WorkspacePackage {
            name,
            root_dir,
            manifest_path,
            is_lib,
            lib_src,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn path_relative(root: &Path, path: &Path) -> String {
    let root_n = normalize_path_display(root);
    let path_n = normalize_path_display(path);
    let rel = path_n
        .strip_prefix(&root_n)
        .map(|s| s.trim_start_matches('/'))
        .filter(|s| !s.is_empty())
        .unwrap_or(path_n.as_str());
    // Prefer relative; if still absolute (drive letter), keep basename-ish full path_n
    // only when strip failed entirely.
    if rel == path_n.as_str() {
        // Case-insensitive retry for Windows drive paths.
        let root_l = root_n.to_ascii_lowercase();
        let path_l = path_n.to_ascii_lowercase();
        if let Some(suffix) = path_l.strip_prefix(&root_l) {
            let start = root_n.len();
            let sliced = path_n.get(start..).unwrap_or(suffix);
            return sliced.trim_start_matches('/').to_string();
        }
    }
    rel.to_string()
}

fn now_stamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("{}s-unix", d.as_secs()),
        Err(_) => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemKind;

    #[test]
    fn parse_basic_pub_items() {
        let src = r#"
/// docs
pub const VERSION: &str = "0.1.0";
pub type Result<T> = std::result::Result<T, ()>;
pub struct Foo {
    x: u32,
}
pub enum Bar { A, B }
pub trait Baz { fn f(&self); }
pub fn free_fn() {}
pub mod nested {}
pub use crate::other::Thing;
"#;
        let items = parse_public_items(src, "demo", "src/lib.rs");
        let names: Vec<_> = items.iter().map(|i| i.item.as_str()).collect();
        assert!(names.contains(&"VERSION"));
        assert!(names.contains(&"Result"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"Bar"));
        assert!(names.contains(&"Baz"));
        assert!(names.contains(&"free_fn"));
        assert!(names.contains(&"nested"));
        assert!(names
            .iter()
            .any(|n| n.contains("Thing") || n.contains("other")));
        assert!(items.iter().all(|i| i.visibility.is_public_api()));
        assert_eq!(
            items.iter().find(|i| i.item == "Foo").unwrap().kind,
            ItemKind::Struct
        );
    }

    #[test]
    fn parse_pub_crate_is_restricted() {
        let src = "pub(crate) fn internal() {}\npub(super) struct S;\n";
        let items = parse_public_items(src, "demo", "src/lib.rs");
        assert_eq!(items.len(), 2);
        assert!(!items[0].visibility.is_public_api());
        assert!(!items[1].visibility.is_public_api());
        match &items[0].visibility {
            Visibility::Restricted(s) => assert!(s.contains("crate")),
            Visibility::Pub => panic!("expected restricted"),
        }
    }

    #[test]
    fn skips_cfg_test_module() {
        let src = r#"
pub fn real() {}

#[cfg(test)]
mod tests {
    pub fn not_public_api() {}
    #[test]
    fn t() {}
}

pub fn also_real() {}
"#;
        let items = parse_public_items(src, "demo", "src/lib.rs");
        let names: Vec<_> = items.iter().map(|i| i.item.as_str()).collect();
        assert!(names.contains(&"real"));
        assert!(names.contains(&"also_real"));
        assert!(!names.contains(&"not_public_api"));
        assert!(!names.contains(&"tests")); // cfg(test) mod skipped
    }

    #[test]
    fn parse_async_unsafe_fn() {
        let src = "pub async fn go() {}\npub unsafe fn raw() {}\npub const fn c() -> u8 { 1 }\n";
        let items = parse_public_items(src, "demo", "src/lib.rs");
        let names: Vec<_> = items.iter().map(|i| i.item.as_str()).collect();
        assert_eq!(names, vec!["go", "raw", "c"]);
        assert!(items.iter().all(|i| i.kind == ItemKind::Fn));
    }

    #[test]
    fn parse_impl_methods() {
        let src = r#"
pub struct DevicePublicId;

impl DevicePublicId {
    pub fn generate() -> Self { Self }
    pub fn parse(input: &str) -> Self { Self }
    fn private() {}
}
"#;
        let items = parse_public_items(src, "demo", "src/device_id.rs");
        let names: Vec<_> = items.iter().map(|i| i.item.as_str()).collect();
        assert!(names.contains(&"DevicePublicId"));
        assert!(names.contains(&"DevicePublicId::generate"));
        assert!(names.contains(&"DevicePublicId::parse"));
        assert!(!names.iter().any(|n| n.ends_with("private")));
    }

    #[test]
    fn parse_pub_item_line_rejects_non_pub() {
        assert!(parse_pub_item_line("fn private() {}").is_none());
        assert!(parse_pub_item_line("public fn nope() {}").is_none());
        assert!(parse_pub_item_line("pub fn ok() {}").is_some());
    }

    #[test]
    fn source_has_test_attr_detects() {
        assert!(source_has_test_attr("#[test]\nfn t() {}"));
        assert!(source_has_test_attr("    #[tokio::test]\nasync fn t() {}"));
        assert!(!source_has_test_attr("pub fn not_a_test() {}"));
    }

    #[test]
    fn package_has_tests_on_temp_tree() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("lib.rs"), "pub fn x() {}\n").unwrap();
        assert!(!package_has_tests(dir.path()).unwrap());
        fs::write(
            src.join("lib.rs"),
            "pub fn x() {}\n#[cfg(test)]\nmod tests {\n#[test]\nfn t() {}\n}\n",
        )
        .unwrap();
        assert!(package_has_tests(dir.path()).unwrap());
    }

    #[test]
    fn multi_line_pub_use_expands_imports() {
        let src = r#"
pub use challenge::{
    compute_challenge_mac, respond_to_challenge, AuthChallenge,
    ChallengeNonce, HOST_SECRET_MIN_LEN,
};
pub use error::{AuthError, Result};
pub use input::{
    modifiers, InputEvent, MouseWheel,
};
"#;
        let items = parse_public_items(src, "demo", "src/lib.rs");
        let uses: Vec<_> = items
            .iter()
            .filter(|i| i.kind == ItemKind::Use)
            .map(|i| i.item.as_str())
            .collect();
        assert!(
            uses.contains(&"challenge::compute_challenge_mac"),
            "got {uses:?}"
        );
        assert!(uses.contains(&"challenge::AuthChallenge"));
        assert!(uses.contains(&"challenge::HOST_SECRET_MIN_LEN"));
        assert!(uses.contains(&"error::AuthError"));
        assert!(uses.contains(&"error::Result"));
        assert!(uses.contains(&"input::modifiers"));
        assert!(uses.contains(&"input::InputEvent"));
        assert!(uses.contains(&"input::MouseWheel"));
        // No truncated brace-open fragments.
        assert!(
            !uses
                .iter()
                .any(|u| u.ends_with("::{") || *u == "challenge::{"),
            "truncated use still present: {uses:?}"
        );
    }

    #[test]
    fn raw_string_fixture_pub_not_inventoried() {
        let src = r##"
pub fn real_api() {}

fn helper() {
    let _fixture = r#"
pub fn not_a_real_export() {}
pub struct FakeStruct;
"#;
}

pub fn also_real() {}
"##;
        let items = parse_public_items(src, "demo", "src/lib.rs");
        let names: Vec<_> = items.iter().map(|i| i.item.as_str()).collect();
        assert!(names.contains(&"real_api"));
        assert!(names.contains(&"also_real"));
        assert!(
            !names.contains(&"not_a_real_export"),
            "raw-string fixture leaked: {names:?}"
        );
        assert!(!names.contains(&"FakeStruct"));
    }

    #[test]
    fn skips_cfg_all_test_module() {
        let src = r#"
pub fn real() {}

#[cfg(all(test, feature = "extra"))]
mod tests {
    pub fn leak() {}
}

pub fn still_real() {}
"#;
        let items = parse_public_items(src, "demo", "src/lib.rs");
        let names: Vec<_> = items.iter().map(|i| i.item.as_str()).collect();
        assert!(names.contains(&"real"));
        assert!(names.contains(&"still_real"));
        assert!(
            !names.contains(&"leak"),
            "cfg(all(test,...)) leaked: {names:?}"
        );
        assert!(is_cfg_test_attr("#[cfg(all(test, feature = \"extra\"))]"));
        assert!(is_cfg_test_attr("#[cfg( all( test , feature = \"x\" ) )]"));
        assert!(!is_cfg_test_attr("#[cfg(not(test))]"));
    }

    #[test]
    fn inventory_workspace_temp_crate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["demo"]
resolver = "2"
"#,
        )
        .unwrap();
        let demo = root.join("demo");
        fs::create_dir_all(demo.join("src")).unwrap();
        fs::write(
            demo.join("Cargo.toml"),
            r#"[package]
name = "demo-lib"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        fs::write(
            demo.join("src/lib.rs"),
            "pub fn covered() {}\npub const FLAG: u8 = 1;\n",
        )
        .unwrap();

        let inv = inventory_workspace(root).expect("inventory");
        assert_eq!(inv.packages.len(), 1);
        assert_eq!(inv.packages[0].name, "demo-lib");
        assert!(inv.packages[0].is_lib);
        assert!(!inv.packages[0].has_tests);
        assert!(inv.packages[0].public_item_count >= 2);
        let names: Vec<_> = inv.items.iter().map(|i| i.item.as_str()).collect();
        assert!(names.contains(&"covered"));
        assert!(names.contains(&"FLAG"));
    }

    #[test]
    fn expand_use_single_and_glob() {
        assert_eq!(
            expand_use_imports("error::ProtocolError;"),
            vec!["error::ProtocolError".to_string()]
        );
        assert_eq!(expand_use_imports("foo::*;"), vec!["foo::*".to_string()]);
    }
}
