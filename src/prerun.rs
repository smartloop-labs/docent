//! The pre-run skill's lookup: before code is written, the current API of
//! the libraries the change uses, so the model writes against the versions
//! the project builds with rather than the ones it remembers.
//!
//! The libraries are the project's dependencies, from its manifest, at the
//! versions its lockfile pins, that the prompt names or the files the agent
//! is shown use; a few at most. Each is searched on the web through the
//! platform (a credit a search) and its top pages read as text, then kept
//! in `.docent/cache/api/` for a week, so the next turns that use it don't
//! search again. The cache is the project's own and never committed: it
//! gets a `.gitignore` of its own when it's made.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::blocking::Client;

/// A library the project depends on, at the version it builds with when
/// that's known.
#[derive(Debug, Clone, PartialEq)]
pub struct Dep {
    pub name: String,
    pub version: Option<String>,
}

impl std::fmt::Display for Dep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.version {
            Some(version) => write!(f, "{} {}", self.name, version),
            None => write!(f, "{}", self.name),
        }
    }
}

/// Libraries looked up for one turn, at most: each is a search.
const LOOKED_UP: usize = 3;
/// Pages read for each library.
const PAGES: usize = 2;
/// Characters of reference kept for each library.
const KEPT: usize = 6_000;
/// How long a library's reference is good for before it's searched again.
const FRESH: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A search, or a page, that takes longer is given up on.
const TIMEOUT: Duration = Duration::from_secs(15);
/// Where the references are kept, under the project's root.
const CACHE: &str = ".docent/cache";

/// The libraries `root` depends on, a project of `kind`, by its manifest.
pub fn dependencies(root: &Path, kind: &str) -> Vec<Dep> {
    let read = |name: &str| fs::read_to_string(root.join(name)).unwrap_or_default();
    match kind {
        "Rust" => cargo(&read("Cargo.toml"), &read("Cargo.lock")),
        "JavaScript/TypeScript" => npm(&read("package.json"), &read("package-lock.json")),
        "Go" => go(&read("go.mod")),
        "Python" => python(&read("requirements.txt"), &read("pyproject.toml")),
        _ => Vec::new(),
    }
}

/// A Cargo.toml's dependencies, at the versions its Cargo.lock pins.
fn cargo(manifest: &str, lock: &str) -> Vec<Dep> {
    let mut deps = Vec::new();
    let mut listing = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            listing = matches!(line, "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]");
            continue;
        }
        let Some((name, spec)) = line.split_once('=').filter(|_| listing) else { continue };
        let name = name.trim().trim_matches('"').to_string();
        // `"1.0"`, or `{ version = "1.0", … }`.
        let wanted = match spec.trim() {
            spec if spec.starts_with('"') => quoted(spec),
            spec => spec.split_once("version").and_then(|(_, rest)| quoted(rest.trim_start_matches([' ', '=']))),
        };
        let version = locked(lock, &name).or(wanted);
        if !name.is_empty() && !deps.iter().any(|d: &Dep| d.name == name) {
            deps.push(Dep { name, version });
        }
    }
    deps
}

/// The version Cargo.lock pins `name` at.
fn locked(lock: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = lock.lines().map(str::trim).collect();
    let at = lines.iter().position(|line| *line == format!("name = \"{name}\""))?;
    lines.get(at + 1).and_then(|line| line.strip_prefix("version = ")).and_then(quoted)
}

/// The text between the first pair of double quotes in `text`.
fn quoted(text: &str) -> Option<String> {
    let rest = text.split_once('"')?.1;
    Some(rest.split_once('"')?.0.to_string()).filter(|s| !s.is_empty())
}

/// A package.json's dependencies, at the versions its lockfile installs.
fn npm(manifest: &str, lock: &str) -> Vec<Dep> {
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(manifest) else { return Vec::new() };
    let lock = serde_json::from_str::<serde_json::Value>(lock).unwrap_or_default();
    let mut deps = Vec::new();
    for listed in ["dependencies", "devDependencies"] {
        let Some(listed) = manifest[listed].as_object() else { continue };
        for (name, wanted) in listed {
            let installed = lock["packages"][format!("node_modules/{name}")]["version"].as_str();
            let wanted = wanted.as_str().map(|v| v.trim_start_matches(['^', '~', '=', '>', '<', ' ']));
            let version = installed.or(wanted).filter(|v| !v.is_empty()).map(str::to_string);
            if !deps.iter().any(|d: &Dep| d.name == *name) {
                deps.push(Dep { name: name.clone(), version });
            }
        }
    }
    deps
}

/// A go.mod's requirements, which are the versions it builds with.
fn go(manifest: &str) -> Vec<Dep> {
    let mut deps = Vec::new();
    let mut block = false;
    for line in manifest.lines().map(str::trim) {
        let required = if block {
            if line == ")" {
                block = false;
                continue;
            }
            line
        } else if line == "require (" {
            block = true;
            continue;
        } else if let Some(one) = line.strip_prefix("require ") {
            one
        } else {
            continue;
        };
        let mut words = required.split_whitespace();
        if let (Some(name), Some(version)) = (words.next(), words.next())
            && !name.starts_with("//")
        {
            deps.push(Dep { name: name.to_string(), version: Some(version.to_string()) });
        }
    }
    deps
}

/// The requirements in requirements.txt and pyproject.toml's dependency
/// list, at the version pinned with `==` when one is.
fn python(requirements: &str, pyproject: &str) -> Vec<Dep> {
    let mut specs: Vec<String> = requirements.lines().map(str::to_string).collect();
    let mut listing = false;
    for line in pyproject.lines().map(str::trim) {
        if line.starts_with("dependencies") && line.contains('[') {
            listing = true;
        }
        if listing {
            specs.extend(line.split('"').skip(1).step_by(2).map(str::to_string));
            if line.ends_with(']') {
                listing = false;
            }
        }
    }
    let mut deps = Vec::new();
    for spec in specs {
        let spec = spec.split('#').next().unwrap_or_default().trim();
        if spec.is_empty() || spec.starts_with('-') {
            continue;
        }
        let end = spec.find(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))).unwrap_or(spec.len());
        let name = spec[..end].to_string();
        let version = spec.split_once("==").map(|(_, v)| v.split([';', ',']).next().unwrap_or(v).trim().to_string());
        if !name.is_empty() && !deps.iter().any(|d: &Dep| d.name == name) {
            deps.push(Dep { name, version });
        }
    }
    deps
}

/// Of `deps`, those the turn uses: the ones the `prompt` names, or when
/// it names none, those the files it's `shown` use, in their order. Every
/// library a shown file imports isn't one the change needs, so they're
/// only a guess. `LOOKED_UP` at most.
pub fn relevant(deps: &[Dep], prompt: &str, shown: &[&str]) -> Vec<Dep> {
    let named = used_in(deps, &[prompt]);
    if !named.is_empty() {
        return named;
    }
    used_in(deps, shown)
}

/// Of `deps`, those `texts` use, in their order; `LOOKED_UP` at most.
fn used_in(deps: &[Dep], texts: &[&str]) -> Vec<Dep> {
    let mut found: Vec<Dep> = Vec::new();
    for text in texts {
        let text = text.to_lowercase();
        for dep in deps {
            if found.len() < LOOKED_UP && !found.contains(dep) && uses(&text, &dep.name.to_lowercase()) {
                found.push(dep.clone());
            }
        }
    }
    found
}

/// Whether `text` names the library `name` as a word of its own, as code
/// does: `serde-json` as `serde_json` too.
fn uses(text: &str, name: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    [name.to_string(), name.replace('-', "_")].iter().any(|name| {
        text.match_indices(name.as_str()).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + name.len()..].chars().next();
            !before.is_some_and(word) && !after.is_some_and(word)
        })
    })
}

/// Where `dep`'s reference is kept in `root`.
fn cached_at(root: &Path, dep: &Dep) -> PathBuf {
    let version = dep.version.as_deref().unwrap_or("latest");
    let file = format!("{}@{}.md", dep.name, version).replace(['/', '\\', ':'], "_");
    root.join(CACHE).join("api").join(file)
}

/// `dep`'s reference kept in `root`, while it's fresh.
fn cached(root: &Path, dep: &Dep) -> Option<String> {
    let path = cached_at(root, dep);
    let age = fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
    (age < FRESH).then(|| fs::read_to_string(&path).ok()).flatten().filter(|text| !text.trim().is_empty())
}

/// Keep `dep`'s `reference` in `root`, the cache made with a `.gitignore`
/// of its own if need be, so it's never committed.
fn keep(root: &Path, dep: &Dep, reference: &str) -> std::io::Result<()> {
    let cache = root.join(CACHE);
    let path = cached_at(root, dep);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let ignore = cache.join(".gitignore");
    if !ignore.exists() {
        fs::write(ignore, "*\n")?;
    }
    fs::write(path, reference)
}

/// The current API reference for each of `deps` in `root`: kept from an
/// earlier turn, else, when `search` is on, searched for and read from the
/// web, and kept. Those with none are left out.
pub fn look_up(client: &Client, root: &Path, deps: &[Dep], search: bool) -> Vec<(Dep, String)> {
    deps.iter()
        .filter_map(|dep| {
            if let Some(reference) = cached(root, dep) {
                return Some((dep.clone(), reference));
            }
            if !search {
                return None;
            }
            let reference = search_for(client, dep)?;
            let _ = keep(root, dep, &reference);
            Some((dep.clone(), reference))
        })
        .collect()
}

/// `dep`'s API reference from the web: its top pages, as text.
fn search_for(client: &Client, dep: &Dep) -> Option<String> {
    let query = format!("{dep} API reference documentation");
    let count = PAGES.to_string();
    let found = crate::usage::platform_get(client, "/v1/websearch", &[("q", &query), ("count", &count)], TIMEOUT)?;
    // A list of URLs, or of results with one each.
    let urls: Vec<String> = found
        .as_array()
        .or_else(|| found["results"].as_array())?
        .iter()
        .filter_map(|r| r.as_str().or_else(|| r["url"].as_str()).map(str::to_string))
        .take(PAGES)
        .collect();
    let mut reference = String::new();
    for url in urls {
        let Some(page) = client.get(&url).timeout(TIMEOUT).send().ok().filter(|r| r.status().is_success()) else {
            continue;
        };
        let text = text_of(&page.text().unwrap_or_default());
        if !text.is_empty() {
            reference.push_str(&format!("From {url}:\n{text}\n\n"));
        }
        if reference.len() >= KEPT {
            break;
        }
    }
    let reference = head(reference.trim(), KEPT);
    (!reference.is_empty()).then_some(reference)
}

/// The first `n` characters of `text`.
fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// The most the reference for `n` of a project's libraries takes of the
/// system message, `LOOKED_UP` of them at most: kept out of the budget
/// for the project's files, so they and it fit the model's window.
pub fn room(n: usize) -> usize {
    match n.min(LOOKED_UP) {
        0 => 0,
        n => crate::skills::body(crate::skills::PRE_RUN).len() + n * (KEPT + HEADING),
    }
}

/// A reference's heading for one library, its name and version, at most.
const HEADING: usize = 100;

/// What `found` adds to the agent's system message: the pre-run skill's
/// text and each library's reference; nothing when nothing was found.
pub fn reference(found: &[(Dep, String)]) -> Option<String> {
    if found.is_empty() {
        return None;
    }
    let libraries: Vec<String> = found.iter().map(|(dep, text)| format!("## {dep}\n\n{text}")).collect();
    Some(format!("{}\n\n{}", crate::skills::body(crate::skills::PRE_RUN), libraries.join("\n\n")))
}

/// Elements whose contents aren't the page's text.
const SKIPPED: [&str; 8] = ["script", "style", "noscript", "svg", "nav", "header", "footer", "head"];
/// Elements that start a line of their own.
const BLOCKS: [&str; 17] =
    ["p", "div", "br", "li", "tr", "pre", "h1", "h2", "h3", "h4", "h5", "h6", "section", "article", "dt", "dd", "table"];

/// The text of a web page `html`: its tags and scripts out, its entities
/// decoded, a line for each block, and no blank runs.
pub fn text_of(html: &str) -> String {
    let lower = html.to_lowercase();
    let mut text = String::new();
    let mut at = 0;
    while let Some(open) = html[at..].find('<').map(|i| at + i) {
        text.push_str(&html[at..open]);
        let Some(close) = html[open..].find('>').map(|i| open + i) else {
            at = html.len();
            break;
        };
        let tag = &lower[open + 1..close];
        let closing = tag.starts_with('/');
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_alphanumeric()).collect();
        at = close + 1;
        if !closing && SKIPPED.contains(&name.as_str()) && !tag.ends_with('/') {
            // On past its end tag, or the page's end without one.
            at = lower[at..].find(&format!("</{name}")).map_or(html.len(), |i| {
                let end = at + i;
                lower[end..].find('>').map_or(html.len(), |j| end + j + 1)
            });
        } else if BLOCKS.contains(&name.as_str()) {
            text.push('\n');
        }
    }
    text.push_str(&html[at..]);
    let text = decode(&text);
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// `text` with the entities a page's text most often has decoded.
fn decode(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(name: &str, version: Option<&str>) -> Dep {
        Dep { name: name.into(), version: version.map(str::to_string) }
    }

    #[test]
    fn reads_a_cargo_projects_dependencies_at_their_locked_versions() {
        let manifest = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\ntokio = \"1.40\"\n\n[dev-dependencies]\ntempfile = \"3\"\n\n[features]\nfast = []\n";
        let lock = "[[package]]\nname = \"serde\"\nversion = \"1.0.219\"\n\n[[package]]\nname = \"tokio\"\nversion = \"1.53.1\"\n";
        assert_eq!(
            cargo(manifest, lock),
            [dep("serde", Some("1.0.219")), dep("tokio", Some("1.53.1")), dep("tempfile", Some("3"))]
        );
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), manifest).unwrap();
        assert_eq!(dependencies(dir.path(), "Rust")[1], dep("tokio", Some("1.40")), "no lockfile: the manifest's");
        assert!(dependencies(dir.path(), "C/C++").is_empty());
    }

    #[test]
    fn reads_npm_go_and_python_dependencies() {
        let manifest = r#"{"dependencies": {"react": "^19.0.0", "zod": "~3.23.0"}, "devDependencies": {"vitest": "latest"}}"#;
        let lock = r#"{"packages": {"node_modules/react": {"version": "19.1.0"}}}"#;
        assert_eq!(
            npm(manifest, lock),
            [dep("react", Some("19.1.0")), dep("zod", Some("3.23.0")), dep("vitest", Some("latest"))]
        );
        let gomod = "module example.com/app\n\ngo 1.23\n\nrequire github.com/spf13/cobra v1.8.1\n\nrequire (\n\tgolang.org/x/sync v0.8.0\n\tgithub.com/x/y v0.1.0 // indirect\n)\n";
        assert_eq!(
            go(gomod),
            [
                dep("github.com/spf13/cobra", Some("v1.8.1")),
                dep("golang.org/x/sync", Some("v0.8.0")),
                dep("github.com/x/y", Some("v0.1.0"))
            ]
        );
        let requirements = "# web\nfastapi==0.115.0\nhttpx>=0.27\n-r dev.txt\n";
        let pyproject = "[project]\ndependencies = [\n  \"pydantic>=2\",\n  \"fastapi\",\n]\n";
        assert_eq!(
            python(requirements, pyproject),
            [dep("fastapi", Some("0.115.0")), dep("httpx", None), dep("pydantic", None)]
        );
    }

    #[test]
    fn looks_up_what_the_prompt_names_else_what_the_files_use() {
        let deps = [dep("serde", None), dep("serde-json", None), dep("tokio", None), dep("clap", None), dep("anyhow", None)];
        let shown = ["use serde_json::Value;\nuse anyhow::Result;", "tokio::spawn(clap)"];
        // Named in the prompt, it's the one, whatever the files use.
        assert_eq!(relevant(&deps, "Add a --verbose flag with clap", &shown), [dep("clap", None)]);
        // Named nowhere, what the files use, in their order.
        assert_eq!(
            relevant(&deps, "Add a --verbose flag", &shown),
            [dep("serde-json", None), dep("anyhow", None), dep("tokio", None)]
        );
        // A name inside another word isn't its use.
        assert!(relevant(&deps, "deserialize the clapping", &[]).is_empty());
        // Room is kept for the reference of those that can be looked up.
        assert_eq!(room(0), 0);
        assert!(room(1) > KEPT && room(5) == room(LOOKED_UP));
    }

    #[test]
    fn a_kept_reference_is_used_and_the_cache_is_never_committed() {
        let dir = tempfile::tempdir().unwrap();
        let serde = dep("serde", Some("1.0.219"));
        assert_eq!(cached(dir.path(), &serde), None);
        keep(dir.path(), &serde, "Serialize, Deserialize").unwrap();
        assert_eq!(fs::read_to_string(dir.path().join(".docent/cache/.gitignore")).unwrap(), "*\n");
        assert!(dir.path().join(".docent/cache/api/serde@1.0.219.md").is_file());
        // Kept, it's used with search off and nothing is fetched.
        let found = look_up(&Client::new(), dir.path(), &[serde.clone(), dep("tokio", None)], false);
        assert_eq!(found, [(serde.clone(), "Serialize, Deserialize".to_string())]);
        let told = reference(&found).unwrap();
        assert!(told.starts_with(crate::skills::body(crate::skills::PRE_RUN)));
        assert!(told.ends_with("## serde 1.0.219\n\nSerialize, Deserialize"));
        assert_eq!(reference(&[]), None);
        assert_eq!(
            cached_at(dir.path(), &dep("@types/node", None)),
            dir.path().join(".docent/cache/api/@types_node@latest.md")
        );
    }

    #[test]
    fn a_page_is_read_as_its_text() {
        let html = "<html><head><title>x</title><style>p{}</style></head><body><nav>Home | Docs</nav>\
                    <h1>Module   fs</h1><p>Read a file with <code>fs::read_to_string</code>.</p>\
                    <script>var a = '<p>';</script><pre>let s = fs::read_to_string(&amp;path)?;</pre>\
                    <footer>©</footer></body></html>";
        assert_eq!(text_of(html), "Module fs\nRead a file with fs::read_to_string.\nlet s = fs::read_to_string(&path)?;");
        assert_eq!(text_of("no tags &lt;here&gt;"), "no tags <here>");
        assert_eq!(text_of("cut <b"), "cut");
    }
}
