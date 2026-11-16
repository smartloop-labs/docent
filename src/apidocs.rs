//! The API of a project's libraries, read from the packages installed for
//! it: their doc comments and the signatures they document, at the exact
//! versions the project builds with, and offline. The pre-run skill keeps
//! them in the project's cache and tells the agent the parts a prompt uses.

use std::fs;
use std::path::{Path, PathBuf};

use crate::prerun::{Dep, locked};

/// One part of a library's API: a module's docs, or a documented item's
/// signature and docs, under its name.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub title: String,
    pub text: String,
}

/// Source files read for one library, at most.
const FILES: usize = 500;
/// A source file bigger than this isn't read.
const FILE_BYTES: u64 = 512 * 1024;
/// A signature runs on this many lines at most.
const SIGNATURE_LINES: usize = 8;

/// `dep`'s API, from the package installed for `root`, a project of
/// `kind`; `None` when it isn't installed, or documents nothing.
#[allow(dead_code)] // The pre-run lookup reads it next.
pub fn collect(root: &Path, kind: &str, dep: &Dep) -> Option<Vec<Section>> {
    match kind {
        "Rust" => {
            let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap_or_default();
            rust(&cargo_registry()?, &lock, dep)
        }
        _ => None,
    }
}

/// The user's home folder.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// Where cargo unpacks the crates it downloads, a folder per registry.
fn cargo_registry() -> Option<PathBuf> {
    let cargo = match std::env::var_os("CARGO_HOME") {
        Some(cargo) => PathBuf::from(cargo),
        None => home()?.join(".cargo"),
    };
    Some(cargo.join("registry").join("src"))
}

/// A crate's API from its source under `registry`, with that of the
/// crates its root re-exports, as clap does clap_builder's, at the
/// versions `lock` pins.
fn rust(registry: &Path, lock: &str, dep: &Dep) -> Option<Vec<Section>> {
    let dir = crate_dir(registry, &dep.name, dep.version.as_deref()?)?;
    let mut sections = crate_api(&dir, &dep.name);
    let lib = fs::read_to_string(dir.join("src").join("lib.rs")).unwrap_or_default();
    // Not the crate itself, as anyhow's `pub use anyhow as format_err`.
    let own = dep.name.replace('-', "_");
    for name in reexported(&lib).into_iter().filter(|name| *name != own) {
        // Named in code as `clap_builder`, maybe published as `clap-builder`.
        let published = [name.clone(), name.replace('_', "-")].into_iter().find_map(|n| Some((locked(lock, &n)?, n)));
        if let Some((version, published)) = published
            && let Some(dir) = crate_dir(registry, &published, &version)
        {
            sections.extend(crate_api(&dir, &name));
        }
    }
    (!sections.is_empty()).then_some(sections)
}

/// The folder `name` `version` is unpacked in, under one of `registry`'s.
fn crate_dir(registry: &Path, name: &str, version: &str) -> Option<PathBuf> {
    fs::read_dir(registry)
        .ok()?
        .flatten()
        .map(|index| index.path().join(format!("{name}-{version}")))
        .find(|dir| dir.join("src").is_dir())
}

/// The crates a crate root's `pub use` lines re-export from.
fn reexported(lib: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in lib.lines().map(str::trim) {
        let Some(path) = line.strip_prefix("pub use ") else { continue };
        let name: String = path.trim_start_matches("::").chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        let local = ["crate", "self", "super", "std", "core", "alloc"].contains(&name.as_str());
        if !name.is_empty() && !local && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The API a crate unpacked in `dir` documents, its root first.
fn crate_api(dir: &Path, name: &str) -> Vec<Section> {
    let src = dir.join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    // The root first: its docs say what the crate is.
    files.sort_by_key(|f| (f.file_name().is_none_or(|n| n != "lib.rs") || f.parent() != Some(src.as_path()), f.clone()));
    let mut sections = Vec::new();
    for file in files.into_iter().take(FILES) {
        if fs::metadata(&file).is_ok_and(|m| m.len() > FILE_BYTES) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else { continue };
        let module = module_path(&src, &file, &name.replace('-', "_"));
        sections.extend(rust_sections(&text, &module, file.parent().unwrap_or(&src)));
    }
    sections
}

/// The `.rs` files under `dir`, at any depth.
fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
}

/// The module `file` under `src` is, as `clap::_derive`.
fn module_path(src: &Path, file: &Path, name: &str) -> String {
    let rel = file.strip_prefix(src).unwrap_or(file).with_extension("");
    let mut parts = vec![name.to_string()];
    parts.extend(rel.iter().map(|p| p.to_string_lossy().into_owned()).filter(|p| !matches!(p.as_str(), "lib" | "main" | "mod")));
    parts.join("::")
}

/// What a Rust source file documents: the `module`'s own docs, and each
/// public item with a doc comment, its signature and docs. Hidden items
/// aren't the API; `dir` is the file's folder, its `include_str!`s from.
fn rust_sections(text: &str, module: &str, dir: &Path) -> Vec<Section> {
    let lines: Vec<&str> = text.lines().collect();
    let mut sections = Vec::new();
    let mut module_doc = Vec::new();
    // In a `/*! … */` block, as regex writes its crate's docs.
    let mut block = false;
    for line in lines.iter().map(|l| l.trim_start()) {
        if block || line.starts_with("/*!") {
            let text = line.strip_prefix("/*!").unwrap_or(line);
            block = !text.contains("*/");
            let text = text.split("*/").next().unwrap_or_default();
            if !(text.trim().is_empty() && module_doc.is_empty()) {
                module_doc.push(text.to_string());
            }
        } else if let Some(doc) = line.strip_prefix("//!") {
            module_doc.push(doc.strip_prefix(' ').unwrap_or(doc).to_string());
        } else if let Some(file) = line.strip_prefix("#![doc = include_str!(\"").and_then(|l| l.split_once('"')).map(|(f, _)| f) {
            module_doc.extend(fs::read_to_string(dir.join(file)).unwrap_or_default().lines().map(str::to_string));
        }
    }
    if let Some(doc) = clean(&module_doc) {
        sections.push(Section { title: module.to_string(), text: doc });
    }
    // The type an `impl` or trait block is for, its methods named after it,
    // and how far the block is indented: one in a `mod` block is too.
    let mut owner: Option<(String, bool, usize)> = None;
    let mut doc: Vec<String> = Vec::new();
    let mut hidden = false;
    for (i, line) in lines.iter().enumerate() {
        let item = line.trim_start();
        if let Some(text) = item.strip_prefix("///").filter(|_| !item.starts_with("////")) {
            doc.push(text.strip_prefix(' ').unwrap_or(text).to_string());
            continue;
        }
        if item.starts_with("#[") {
            hidden |= item.contains("doc(hidden)");
            continue;
        }
        let indent = line.len() - item.len();
        if owner.as_ref().is_some_and(|(_, _, at)| indent <= *at && item.starts_with('}')) {
            owner = None;
        }
        // Not one inside another's block, nor one closed on its own line.
        if owner.as_ref().is_none_or(|(_, _, at)| indent <= *at) && !item.trim_end().ends_with('}') {
            if let Some(name) = impl_type(item) {
                owner = Some((name, false, indent));
            } else if let Some(name) = item.strip_prefix("pub trait ").map(ident) {
                owner = Some((name, true, indent));
            }
        }
        let in_trait = owner.as_ref().is_some_and(|(_, t, _)| *t);
        if let (Some(name), Some(docs)) = (item_name(item, in_trait), clean(&doc))
            && !hidden
        {
            let method = item.contains("fn ") && owner.as_ref().is_some_and(|(_, _, at)| indent > *at);
            let title = match &owner {
                Some((owner, _, _)) if method => format!("{owner}::{name}"),
                _ => name,
            };
            sections.push(Section { title, text: format!("`{}`\n\n{docs}", signature(&lines[i..])) });
        }
        doc.clear();
        hidden = false;
    }
    sections
}

/// The type an `impl` line is for: `Command` of `impl Command {`, or of
/// `impl<T> Display for Command<T>`.
fn impl_type(line: &str) -> Option<String> {
    let rest = line.strip_prefix("impl")?;
    let rest = match rest.strip_prefix('<') {
        Some(generics) => {
            let mut depth = 1;
            let end = generics.char_indices().find(|(_, c)| {
                depth += match c {
                    '<' => 1,
                    '>' => -1,
                    _ => 0,
                };
                depth == 0
            })?;
            &generics[end.0 + 1..]
        }
        None if rest.starts_with(' ') => rest,
        None => return None,
    };
    let ty = rest.split_once(" for ").map_or(rest, |(_, ty)| ty).trim();
    let name = ident(ty.rsplit("::").next().unwrap_or(ty));
    (!name.is_empty()).then_some(name)
}

/// The name a public item line declares, as `arg` of `pub fn arg(`: any
/// in a trait, whose items are all public, and `arg!` of a macro.
fn item_name(line: &str, in_trait: bool) -> Option<String> {
    if let Some(name) = line.strip_prefix("macro_rules! ") {
        return Some(format!("{}!", ident(name)));
    }
    let mut rest = match line.strip_prefix("pub ") {
        Some(rest) => rest,
        None if in_trait => line,
        None => return None,
    };
    // `const fn`, but not a `const` item.
    while let Some(after) = ["async ", "unsafe ", "extern \"C\" "]
        .iter()
        .find_map(|q| rest.strip_prefix(q))
        .or_else(|| rest.strip_prefix("const ").filter(|r| r.starts_with("fn ") || r.starts_with("unsafe ")))
    {
        rest = after;
    }
    let (keyword, name) = rest.split_once(' ')?;
    let kinds = ["fn", "struct", "enum", "trait", "type", "const", "static", "union"];
    let name = ident(name);
    (kinds.contains(&keyword) && !name.is_empty()).then_some(name)
}

/// The identifier `text` starts with.
fn ident(text: &str) -> String {
    text.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect()
}

/// An item's signature from its first `lines`: up to its body, on one line.
fn signature(lines: &[&str]) -> String {
    let mut words = Vec::new();
    for line in lines.iter().take(SIGNATURE_LINES) {
        let line = line.trim();
        if let Some((head, _)) = line.split_once('{') {
            words.push(head.trim());
            break;
        }
        words.push(line.trim_end_matches(';'));
        if line.ends_with(';') {
            break;
        }
    }
    words.iter().filter(|w| !w.is_empty()).copied().collect::<Vec<_>>().join(" ").replace("( ", "(").replace(" )", ")")
}

/// Doc comment `lines` as text: with the lines a doctest hides out of its
/// examples, and no blank lines at its ends; `None` when there's nothing.
fn clean(lines: &[String]) -> Option<String> {
    let mut fenced = false;
    let mut kept: Vec<&str> = Vec::new();
    for line in lines {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if fenced && (line.trim() == "#" || line.trim_start().starts_with("# ")) {
            continue;
        }
        kept.push(line);
    }
    let text = kept.join("\n").trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn a_crate_s_api_is_its_docs_and_what_they_document() {
        let registry = tempfile::tempdir().unwrap();
        let index = registry.path().join("index.crates.io-1");
        let app = index.join("app-1.2.0");
        write(&app.join("README.md"), "# app\n\nParses arguments.");
        write(
            &app.join("src/lib.rs"),
            "//! The app crate.\n#![doc = include_str!(\"../README.md\")]\n\npub use app_core::*;\npub use crate::derive::Parser;\npub use app as other_app;\n\n\
             /// Run it.\n///\n/// ```\n/// # use app::run;\n/// run(1);\n/// ```\n#[must_use]\npub fn run(\n    times: u32,\n) -> bool {\n    true\n}\n\n\
             pub fn undocumented() {}\n\n/// Internal.\n#[doc(hidden)]\npub fn secret() {}\n\n/// Not public.\nfn private() {}\n\n\
             /// Parses.\npub trait Parser {\n    /// Parse the args.\n    fn parse() -> Self;\n}\n\n\
             /// Says hello.\n#[macro_export]\nmacro_rules! hello {\n    () => {};\n}\n",
        );
        write(&app.join("src/derive/mod.rs"), "//! Derive reference: `#[arg(short, long)]`.\n");
        let core = index.join("app-core-0.3.1");
        // Docs in a block, and an `impl` in a `mod`.
        write(
            &core.join("src/lib.rs"),
            "/*!\nCore of app.\n*/\n\npub(crate) mod string {\n    /// Builds.\n    pub struct Builder;\n\n    impl Builder {\n        /// Makes one.\n        pub fn new() -> Builder {\n            Builder\n        }\n    }\n}\n",
        );
        write(
            &core.join("src/builder/command.rs"),
            "pub struct Command;\n\nimpl<'a> Command {\n    /// Adds an argument.\n    pub fn arg(mut self, a: impl Into<Arg>) -> Self {\n        self\n    }\n}\n",
        );
        let lock = "[[package]]\nname = \"app\"\nversion = \"1.2.0\"\n\n[[package]]\nname = \"app-core\"\nversion = \"0.3.1\"\n";
        let app = Dep { name: "app".into(), version: Some("1.2.0".into()) };
        let sections = rust(registry.path(), lock, &app).unwrap();
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        // Itself re-exported isn't read again.
        assert_eq!(titles, ["app", "run", "Parser", "Parser::parse", "hello!", "app::derive", "app_core", "Builder", "Builder::new", "Command::arg"]);
        assert_eq!(sections[6].text, "Core of app.");
        assert_eq!(sections[0].text, "The app crate.\n# app\n\nParses arguments.");
        // A doctest's hidden lines are left out of its example.
        assert_eq!(sections[1].text, "`pub fn run(times: u32,) -> bool`\n\nRun it.\n\n```\nrun(1);\n```");
        assert_eq!(sections[3].text, "`fn parse() -> Self`\n\nParse the args.");
        assert!(sections.last().unwrap().text.starts_with("`pub fn arg(mut self, a: impl Into<Arg>) -> Self`"));
        // Not installed at the version the project builds with: nothing.
        assert_eq!(rust(registry.path(), lock, &Dep { name: "app".into(), version: Some("1.3.0".into()) }), None);
        assert_eq!(rust(registry.path(), lock, &Dep { name: "app".into(), version: None }), None);
    }

    #[test]
    fn an_impl_line_names_its_type() {
        assert_eq!(impl_type("impl Command {").as_deref(), Some("Command"));
        assert_eq!(impl_type("impl<T: Into<String>> Display for crate::Arg<T> {").as_deref(), Some("Arg"));
        assert_eq!(impl_type("implement {"), None);
    }
}
