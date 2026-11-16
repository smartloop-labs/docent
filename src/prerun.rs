//! The pre-run skill's lookup: before code is written, the current API of
//! the libraries the change uses, so the model writes against the versions
//! the project builds with rather than the ones it remembers.
//!
//! The libraries are the project's dependencies, from its manifest, at the
//! versions its lockfile pins, that the prompt names, a few at most; or
//! when it names only a scope several are of, `mui` of `@mui/material`,
//! the one of those that fits the prompt best, if any fits it well. Each
//! one's API is read from the package installed for the project
//! (`apidocs`), offline, and kept in `.docent/cache/api/` as markdown, so
//! the next turns that use it don't read it again; of that, each turn is
//! told the parts its prompt uses. The cache is the project's own and never committed: it gets a
//! `.gitignore` of its own when it's made.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::apidocs::Section;

/// The API of a `dep` a turn is `told`, and how well it `fit` the turn's
/// prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct Api {
    pub dep: Dep,
    pub told: String,
    pub fit: f64,
}

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

/// Libraries looked up for one turn, at most.
const LOOKED_UP: usize = 3;
/// Libraries of a scope a prompt names that are looked up for the one
/// that fits it, at most: only that one is told.
const GUESSED: usize = 15;
/// Characters of API told for each library.
const KEPT: usize = 6_000;
/// A section told longer than this is cut, at a line's end.
const SECTION: usize = 2_000;
/// The fewest sections a library's fit is the mean over: one of only a
/// few, which share a word or two with any prompt, is no fit for that.
const FITTING: usize = 30;
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
        "Java" | "Java/Kotlin" | "Kotlin" => {
            let mut deps = maven(&read("pom.xml"));
            for dep in gradle(&format!("{}\n{}", read("build.gradle"), read("build.gradle.kts"))) {
                if !deps.iter().any(|d| d.name == dep.name) {
                    deps.push(dep);
                }
            }
            deps
        }
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
        let Some((name, _)) = line.split_once('=').filter(|_| listing) else { continue };
        let name = name.trim().trim_matches('"').to_string();
        // Not the manifest's `"1"`: a crate is installed, and read, at the
        // version it's locked at, and one not locked yet isn't installed.
        let version = locked(lock, &name);
        if !name.is_empty() && !deps.iter().any(|d: &Dep| d.name == name) {
            deps.push(Dep { name, version });
        }
    }
    deps
}

/// The version Cargo.lock pins `name` at: of several, the one the
/// project's own packages, those with no `source`, depend on, which Cargo
/// lists with its version (`"base64 0.23.1"`); else its only one.
pub fn locked(lock: &str, name: &str) -> Option<String> {
    let listed = format!("\"{name} ");
    let own = lock.split("[[package]]").filter(|package| !package.contains("\nsource = "));
    if let Some(version) = own
        .flat_map(str::lines)
        .find_map(|line| line.trim().strip_prefix(listed.as_str()))
        .and_then(|rest| rest.split(['"', ' ']).next())
    {
        return Some(version.to_string());
    }
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

/// A pom.xml's dependencies, as `group:artifact`, at their versions, a
/// `${property}` one as its `<properties>` set it.
fn maven(pom: &str) -> Vec<Dep> {
    let tag = |text: &str, name: &str| -> Option<String> {
        let (_, rest) = text.split_once(&format!("<{name}>"))?;
        Some(rest.split_once(&format!("</{name}>"))?.0.trim().to_string()).filter(|v| !v.is_empty())
    };
    let properties = pom.split_once("<properties>").and_then(|(_, p)| p.split_once("</properties>")).map_or("", |(p, _)| p);
    let mut deps = Vec::new();
    for block in pom.split("<dependency>").skip(1) {
        let block = block.split_once("</dependency>").map_or(block, |(b, _)| b);
        let (Some(group), Some(artifact)) = (tag(block, "groupId"), tag(block, "artifactId")) else { continue };
        let version = tag(block, "version").and_then(|v| match v.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
            Some(property) => tag(properties, property),
            None => Some(v),
        });
        let name = format!("{group}:{artifact}");
        if !deps.iter().any(|d: &Dep| d.name == name) {
            deps.push(Dep { name, version });
        }
    }
    deps
}

/// A Gradle build's dependencies: each quoted `group:artifact:version`, the
/// version left out when it's a variable's.
fn gradle(build: &str) -> Vec<Dep> {
    let mut deps = Vec::new();
    for quoted in build.split(['"', '\'']).skip(1).step_by(2) {
        let parts: Vec<&str> = quoted.split(':').collect();
        let coordinate = |p: &&str| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !(2..=3).contains(&parts.len()) || !parts[..2].iter().all(coordinate) || !parts[0].contains('.') {
            continue;
        }
        let name = format!("{}:{}", parts[0], parts[1]);
        let version = parts.get(2).filter(|v| !v.contains('$') && !v.is_empty()).map(|v| v.to_string());
        if !deps.iter().any(|d: &Dep| d.name == name) {
            deps.push(Dep { name, version });
        }
    }
    deps
}

/// Of `deps`, those the turn uses: the ones the `prompt` names, or when
/// it names none, those a word of it is part of the name of, as `yaml` of
/// `js-yaml` and `mui` of `@mui/material`, and whether they're that guess:
/// of a guess, only the one that fits the prompt is told, by `guess`.
/// `LOOKED_UP` named at most, `GUESSED` guessed.
pub fn relevant(deps: &[Dep], prompt: &str) -> (Vec<Dep>, bool) {
    let named = used_in(deps, &[prompt], LOOKED_UP);
    if !named.is_empty() {
        return (named, false);
    }
    let asked: HashSet<String> = prompt.to_lowercase().split(|c: char| !c.is_alphanumeric()).map(|w| singular(w.to_string())).collect();
    // Not `@types`' packages, which only type another.
    let partly = deps
        .iter()
        .filter(|d| !d.name.starts_with("@types/") && name_parts(&d.name).iter().any(|part| asked.contains(part)))
        .take(GUESSED)
        .cloned()
        .collect();
    (partly, true)
}

/// Words of a package's name that say what it's for, `yaml` of `js-yaml`
/// and `mui` and `material` of `@mui/material`: not a short one, nor one
/// that names its language, host or kind rather than its use.
fn name_parts(name: &str) -> Vec<String> {
    const VAGUE: [&str; 16] = [
        "api", "com", "core", "github", "gitlab", "golang", "google", "lib", "node", "org", "plugin", "rust", "sdk", "type", "util",
        "utils",
    ];
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .map(|part| singular(part.to_string()))
        .filter(|part| part.len() >= 3 && !part.chars().all(|c| c.is_ascii_digit()) && !VAGUE.contains(&part.as_str()))
        .collect()
}

/// Of the APIs `found` for libraries a prompt only names part of, the one
/// that fits it best, the first of those that fit it as well. Part of its
/// name in the prompt is a strong sign already, so how well it fits picks
/// between them rather than ruling one out: `js-yaml` for a prompt about
/// yaml whose words its docs don't use.
pub fn guess(found: Vec<Api>) -> Vec<Api> {
    found.into_iter().rev().max_by(|a, b| a.fit.total_cmp(&b.fit)).into_iter().collect()
}

/// Of `deps`, those `texts` use, in their order; `most` at most.
fn used_in(deps: &[Dep], texts: &[&str], most: usize) -> Vec<Dep> {
    let mut found: Vec<Dep> = Vec::new();
    for text in texts {
        let text = text.to_lowercase();
        for dep in deps {
            if found.len() < most && !found.contains(dep) && called(dep).iter().any(|name| uses(&text, name)) {
                found.push(dep.clone());
            }
        }
    }
    found
}

/// The names code calls `dep` by: its own, and for a Go module or a
/// Java one, its last part, `cobra` of `github.com/spf13/cobra` and `gson`
/// of `com.google.code.gson:gson`, and a Java one's group, as imported.
fn called(dep: &Dep) -> Vec<String> {
    let name = dep.name.to_lowercase();
    let mut names = vec![name.clone()];
    // Not a Go module's major version, as `v2` of `…/chi/v5`.
    let version = |p: &&str| p.strip_prefix('v').is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()));
    if let Some(last) = name.rsplit(['/', ':']).find(|p| !version(p)).filter(|last| *last != name && last.len() > 1) {
        names.push(last.to_string());
    }
    if let Some((group, _)) = name.split_once(':') {
        names.push(group.to_string());
    }
    names
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

/// Whether `dep`'s API is kept in `root`, read from its package before.
pub fn kept(root: &Path, dep: &Dep) -> bool {
    cached_at(root, dep).is_file()
}

/// Where `dep`'s API is kept in `root`.
fn cached_at(root: &Path, dep: &Dep) -> PathBuf {
    let version = dep.version.as_deref().unwrap_or("latest");
    let file = format!("{}@{}.md", dep.name, version).replace(['/', '\\', ':'], "_");
    root.join(CACHE).join("api").join(file)
}

/// The line a kept section starts with, its title after it: a comment, as
/// a library's docs have headings of their own.
const MARK: &str = "<!-- section: ";

/// `dep`'s API kept in `root`, read from its package before. A version's
/// API doesn't change, so it's kept for good.
fn cached(root: &Path, dep: &Dep) -> Option<Vec<Section>> {
    let mut sections: Vec<Section> = Vec::new();
    for line in fs::read_to_string(cached_at(root, dep)).ok()?.lines() {
        if let Some(title) = line.strip_prefix(MARK).and_then(|l| l.strip_suffix(" -->")) {
            sections.push(Section { title: title.to_string(), text: String::new() });
        } else if let Some(section) = sections.last_mut() {
            section.text.push_str(line);
            section.text.push('\n');
        }
    }
    sections.iter_mut().for_each(|s| s.text = s.text.trim().to_string());
    (!sections.is_empty()).then_some(sections)
}

/// Keep `dep`'s API `sections` in `root`, as markdown to read, the cache
/// made with a `.gitignore` of its own if need be, so it's never committed.
fn keep(root: &Path, dep: &Dep, sections: &[Section]) -> std::io::Result<()> {
    let cache = root.join(CACHE);
    let path = cached_at(root, dep);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let ignore = cache.join(".gitignore");
    if !ignore.exists() {
        fs::write(ignore, "*\n")?;
    }
    let text: String = sections.iter().map(|s| format!("{MARK}{} -->\n{}\n\n", s.title, s.text)).collect();
    fs::write(path, text)
}

/// The API of each of `deps` in `root`, a project of `kind`, the parts
/// the `prompt` uses: kept from an earlier turn,
/// else, when `read`, read from its installed package and kept. Those
/// with none are left out.
pub fn look_up(root: &Path, kind: &str, deps: &[Dep], prompt: &str, read: bool) -> Vec<Api> {
    deps.iter()
        .filter_map(|dep| {
            let sections = match cached(root, dep) {
                Some(sections) => sections,
                None if read => {
                    let sections = crate::apidocs::collect(root, kind, dep)?;
                    let _ = keep(root, dep, &sections);
                    sections
                }
                None => return None,
            };
            let (told, fit) = select(&sections, &dep.name, prompt);
            Some(Api { dep: dep.clone(), told, fit })
        })
        .collect()
}

/// Of a library's `sections`, what a `prompt` uses, `KEPT` at most: its
/// top-level docs, then the sections with most of its words, a rare word
/// counting for more than one most sections have, and one in a section's
/// title for more than in its text; told in the library's order. With how
/// well the library, `name`d, fits the prompt: the mean score of its
/// sections, over `FITTING` at least.
fn select(sections: &[Section], name: &str, prompt: &str) -> (String, f64) {
    let parts: Vec<Section> = sections.iter().flat_map(split).collect();
    // An item's own name is its title, not the type it's of, which every
    // other method of that type has too: `opts` of `Command.opts`.
    let titles: Vec<HashSet<String>> = parts.iter().map(|p| words(item(&p.title))).collect();
    let texts: Vec<HashSet<String>> = parts.iter().map(|p| words(&format!("{}\n{}", p.title, p.text))).collect();
    let named: HashSet<&String> = titles.iter().flatten().collect();
    // Not the library's own name, or a word of it, which its docs have all
    // over; and a word the API names things with as the end of one asked,
    // `command` of `subcommand`, too.
    let own = singular(name.rsplit(['/', ':']).next().unwrap_or(name).to_lowercase().replace(['-', '_'], ""));
    let mut asked = words(prompt);
    asked.remove(&own);
    for part in name_parts(name) {
        asked.remove(&part);
    }
    let ends: Vec<String> = asked.iter().filter_map(|w| ending(w, &named)).collect();
    asked.extend(ends);
    let rarity: HashMap<&String, f64> = asked
        .iter()
        .map(|word| {
            let used = titles.iter().zip(&texts).filter(|(t, x)| t.contains(word) || x.contains(word)).count();
            (word, (1.0 + parts.len() as f64 / used.max(1) as f64).ln())
        })
        .collect();
    let score = |i: usize| -> f64 {
        let hits: f64 = rarity
            .iter()
            .map(|(word, rare)| match () {
                _ if titles[i].contains(*word) => 3.0 * rare,
                _ if texts[i].contains(*word) => *rare,
                _ => 0.0,
            })
            .sum();
        // A long section has more words by being long.
        hits / (1.0 + parts[i].text.len() as f64 / SECTION as f64)
    };
    let fit = (0..parts.len()).map(score).sum::<f64>() / parts.len().max(FITTING) as f64;
    let mut ranked: Vec<(usize, f64)> = (1..parts.len()).map(|i| (i, score(i))).filter(|(_, s)| *s > 0.0).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let told = |i: usize| format!("### {}\n\n{}", parts[i].title, cut(&parts[i].text, SECTION));
    let mut chosen = vec![0];
    let mut used = told(0).len();
    for (i, _) in ranked {
        let size = told(i).len() + 2;
        if used + size <= KEPT {
            chosen.push(i);
            used += size;
        }
    }
    chosen.sort();
    (chosen.into_iter().filter(|i| *i < parts.len()).map(told).collect::<Vec<_>>().join("\n\n"), fit)
}

/// A section's `title` without the type it's of: `opts` of `Command.opts`
/// and of `Command::opts`, but `clap::_derive` whole.
fn item(title: &str) -> &str {
    match title.split_once("::").or_else(|| title.split_once('.')) {
        Some((owner, name)) if owner.starts_with(char::is_uppercase) && !owner.contains(' ') => name,
        _ => title,
    }
}

/// The longest end of `word` that's a word of its own, `known`, four
/// letters or more: `command` of `subcommand`.
fn ending(word: &str, known: &HashSet<&String>) -> Option<String> {
    (1..word.len().saturating_sub(3))
        .filter(|at| word.is_char_boundary(*at))
        .map(|at| &word[at..])
        .find(|end| known.contains(&end.to_string()))
        .map(str::to_string)
}

/// `section` in parts a turn can be told some of: a long one at each of
/// its own headings, each part titled with the heading it's under.
fn split(section: &Section) -> Vec<Section> {
    if section.text.len() <= SECTION {
        return vec![section.clone()];
    }
    let mut parts = vec![Section { title: section.title.clone(), text: String::new() }];
    let mut fenced = false;
    for line in section.text.lines() {
        fenced ^= line.trim_start().starts_with("```");
        if let Some(heading) = line.strip_prefix('#').filter(|_| !fenced).map(|h| h.trim_start_matches('#').trim()) {
            parts.push(Section { title: format!("{} › {heading}", section.title), text: String::new() });
        }
        let part = parts.last_mut().expect("a part");
        part.text.push_str(line);
        part.text.push('\n');
    }
    parts.retain(|p| !p.text.trim().is_empty());
    parts.iter_mut().for_each(|p| p.text = p.text.trim().to_string());
    parts
}

/// The words of `text`, as code names things: lowercased, each name's
/// parts too (`ArgAction` is `argaction`, `arg` and `action`; so is
/// `arg_action`), three letters or more.
fn words(text: &str) -> HashSet<String> {
    let mut words = HashSet::new();
    for name in text.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|n| !n.is_empty()) {
        let mut part = String::new();
        let mut parts = Vec::new();
        for (i, c) in name.chars().enumerate() {
            if c == '_' || (c.is_uppercase() && i > 0 && !part.chars().last().is_some_and(char::is_uppercase)) {
                parts.push(std::mem::take(&mut part));
            }
            if c != '_' {
                part.extend(c.to_lowercase());
            }
        }
        parts.push(part);
        parts.push(name.to_lowercase().replace('_', ""));
        words.extend(parts.into_iter().filter(|p| p.chars().count() >= 3 && !ASKING.contains(&p.as_str())).map(singular));
    }
    words
}

/// Words a prompt asks with rather than about, in no API's terms.
const ASKING: [&str; 22] = [
    "add", "adds", "make", "makes", "use", "uses", "using", "with", "that", "this", "the", "and", "for", "from", "into",
    "change", "update", "remove", "takes", "take", "please", "should",
];

/// `word` as one of a thing: `flags` as `flag`.
fn singular(word: String) -> String {
    match word.strip_suffix('s') {
        Some(one) if one.len() >= 3 && !one.ends_with('s') => one.to_string(),
        _ => word,
    }
}

/// `text`'s lines up to `n` characters, said to go on when cut, with a
/// code block it cuts into closed.
fn cut(text: &str, n: usize) -> String {
    if text.len() <= n {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut fenced = false;
    for line in text.lines() {
        if kept.len() + line.len() + 1 > n {
            break;
        }
        fenced ^= line.trim_start().starts_with("```");
        kept.push_str(line);
        kept.push('\n');
    }
    if fenced {
        kept.push_str("```\n");
    }
    kept.push('…');
    kept
}

/// What `found` adds to the agent's system message: the pre-run skill's
/// text and each library's API; nothing when nothing was found.
pub fn reference(found: &[Api]) -> Option<String> {
    if found.is_empty() {
        return None;
    }
    let libraries: Vec<String> = found.iter().map(|api| format!("## {}\n\n{}", api.dep, api.told)).collect();
    Some(format!("{}\n\n{}", crate::skills::body(crate::skills::PRE_RUN), libraries.join("\n\n")))
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
            [dep("serde", Some("1.0.219")), dep("tokio", Some("1.53.1")), dep("tempfile", None)]
        );
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), manifest).unwrap();
        assert_eq!(dependencies(dir.path(), "Rust")[1], dep("tokio", None), "no lockfile: no version");
        // Locked at several versions: the one the project's own package uses.
        let lock = "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"base64 0.23.1\",\n \"serde\",\n]\n\n\
                    [[package]]\nname = \"base64\"\nversion = \"0.13.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
                    [[package]]\nname = \"base64\"\nversion = \"0.23.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        assert_eq!(locked(lock, "base64").as_deref(), Some("0.23.1"));
        assert_eq!(locked(lock, "app").as_deref(), Some("0.1.0"));
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
    fn reads_maven_and_gradle_dependencies() {
        let pom = "<project><properties><gson.version>2.11.0</gson.version></properties><dependencies>\
                   <dependency><groupId>com.google.code.gson</groupId><artifactId>gson</artifactId><version>${gson.version}</version></dependency>\
                   <dependency><groupId>org.slf4j</groupId><artifactId>slf4j-api</artifactId></dependency>\
                   </dependencies></project>";
        assert_eq!(maven(pom), [dep("com.google.code.gson:gson", Some("2.11.0")), dep("org.slf4j:slf4j-api", None)]);
        let build = "plugins { id 'java' }\ndependencies {\n    implementation 'com.squareup.okhttp3:okhttp:4.12.0'\n    \
                     testImplementation(\"org.junit.jupiter:junit-jupiter:$junit\")\n    implementation \"not a:coordinate\"\n}\n";
        assert_eq!(gradle(build), [dep("com.squareup.okhttp3:okhttp", Some("4.12.0")), dep("org.junit.jupiter:junit-jupiter", None)]);
    }

    #[test]
    fn looks_up_what_the_prompt_names_else_what_it_names_part_of() {
        let mui = [dep("@mui/material", None), dep("@mui/icons-material", None), dep("@types/react", None), dep("react", None)];
        let deps = [dep("serde", None), dep("serde-json", None), dep("tokio", None), dep("clap", None), dep("anyhow", None)];
        // Named in the prompt, it's the one.
        assert_eq!(relevant(&deps, "Add a --verbose flag with clap"), (vec![dep("clap", None)], false));
        assert_eq!(relevant(&deps, "Read it with serde_json and anyhow"), (vec![dep("serde-json", None), dep("anyhow", None)], false));
        // Named nowhere, none.
        assert_eq!(relevant(&deps, "Add a --verbose flag"), (Vec::new(), true));
        // A name inside another word isn't its use.
        assert!(relevant(&deps, "deserialize the clapping").0.is_empty());
        // Part of its name named, those it's part of, as a guess; never
        // `@types`', nor by a vague part.
        assert_eq!(relevant(&mui, "use MUI's Snackbar"), (vec![dep("@mui/material", None), dep("@mui/icons-material", None)], true));
        assert_eq!(relevant(&mui, "type the props"), (Vec::new(), true));
        let studio = [dep("js-yaml", None), dep("@types/js-yaml", None), dep("crypto-js", None), dep("electron-store", None)];
        assert_eq!(relevant(&studio, "make the theme yaml loading safe"), (vec![dep("js-yaml", None)], true));
        assert_eq!(relevant(&studio, "keep the settings in the store"), (vec![dep("electron-store", None)], true));
        assert_eq!(relevant(&studio, "add a js helper for the api"), (Vec::new(), true));
        // A Go or Java module by the names its code calls it.
        let modules = [dep("github.com/go-chi/chi/v5", None), dep("com.google.code.gson:gson", None), dep("org.slf4j:slf4j-api", None)];
        assert_eq!(relevant(&modules, "route it with chi").0, [modules[0].clone()]);
        assert_eq!(relevant(&modules, "parse it with Gson").0, [modules[1].clone()]);
        assert_eq!(relevant(&modules, "log it with org.slf4j").0, [modules[2].clone()]);
        // Of a guess, only the one that fits the prompt best, the first of
        // those that fit it as well.
        let api = |name: &str, fit| Api { dep: dep(name, None), told: String::new(), fit };
        assert_eq!(guess(vec![api("serde", 0.3), api("clap", 0.6), api("anyhow", 0.0)]), [api("clap", 0.6)]);
        assert_eq!(guess(vec![api("serde", 0.0), api("clap", 0.0)]), [api("serde", 0.0)]);
        assert!(guess(Vec::new()).is_empty());
    }

    fn section(title: &str, text: &str) -> Section {
        Section { title: title.into(), text: text.into() }
    }

    #[test]
    fn a_kept_api_is_used_and_the_cache_is_never_committed() {
        let dir = tempfile::tempdir().unwrap();
        let serde = dep("serde", Some("1.0.219"));
        assert_eq!(cached(dir.path(), &serde), None);
        let sections = [section("serde", "Serialization.\n\n## Derive\n\nUse it."), section("Serialize", "`pub trait Serialize`")];
        keep(dir.path(), &serde, &sections).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join(".docent/cache/.gitignore")).unwrap(), "*\n");
        // Kept as markdown, read back the same, its headings and all.
        assert!(fs::read_to_string(dir.path().join(".docent/cache/api/serde@1.0.219.md")).unwrap().starts_with("<!-- section: serde -->\nSerialization."));
        assert_eq!(cached(dir.path(), &serde).unwrap(), sections);
        // Kept, it's used without reading the package again.
        let found = look_up(dir.path(), "Rust", &[serde.clone(), dep("tokio", None)], "derive Serialize", false);
        assert_eq!(found.len(), 1);
        assert!(found[0].told.starts_with("### serde\n\nSerialization.") && found[0].told.contains("### Serialize"));
        let told = reference(&found).unwrap();
        assert!(told.starts_with(crate::skills::body(crate::skills::PRE_RUN)));
        assert!(told.contains("## serde 1.0.219\n\n### serde"));
        assert_eq!(reference(&[]), None);
        assert_eq!(
            cached_at(dir.path(), &dep("@types/node", None)),
            dir.path().join(".docent/cache/api/@types_node@latest.md")
        );
    }

    #[test]
    fn a_turn_is_told_the_parts_of_an_api_it_uses() {
        let mut sections = vec![section("clap", "Command line argument parser.")];
        for i in 0..40 {
            sections.push(section(&format!("Thing{i}::other"), &format!("`pub fn other{i}()`\n\nUnrelated, {}.", "words ".repeat(40))));
        }
        sections.push(section("Arg::short", "`pub fn short(self, s: char) -> Self`\n\nSets the short flag."));
        sections.push(section("ArgAction", "`pub enum ArgAction`\n\nWhat a flag does: SetTrue."));
        let derive = format!("Derive reference.\n\n## Arg Attributes\n\n`#[arg(short, long)]`\n\n## Command Attributes\n\n{}", "about ".repeat(400));
        sections.push(section("clap::_derive", &derive));
        let (told, fit) = select(&sections, "clap", "add a --shout flag with #[arg(short)] and an action with clap");
        // The top-level docs first, then what's used, in the library's order.
        let titles: Vec<&str> = told.lines().filter_map(|l| l.strip_prefix("### ")).collect();
        assert_eq!(titles[0], "clap");
        assert!(titles.contains(&"Arg::short") && titles.contains(&"ArgAction"), "{titles:?}");
        assert!(titles.contains(&"clap::_derive › Arg Attributes"), "{titles:?}");
        assert!(!titles.iter().any(|t| t.starts_with("Thing")), "{titles:?}");
        assert!(titles.iter().position(|t| *t == "Arg::short") < titles.iter().position(|t| *t == "ArgAction"));
        assert!(told.len() <= KEPT);
        // It fits a prompt that asks of it better than one that doesn't.
        let (_, other) = select(&sections, "clap", "add a test for subtract with zeros");
        assert!(fit > other * 5.0, "{fit} {other}");
        // A library of a section or two that shares a word or two fits no
        // better, nor does one by the words of its own name.
        let readme = [section("@mui/icons-material", "Material icons for MUI, shown as an error or a request's state.")];
        let (_, small) = select(&readme, "@mui/icons-material", "use MUI's Snackbar to show an error when the chat request fails");
        assert!(small * 5.0 < fit, "{small} {fit}");
        // Names in code are words in their parts.
        assert!(words("ArgAction default_value_t").is_superset(&["argaction", "arg", "action", "default", "value", "defaultvaluet"].map(String::from).into()));
        // Not for the library's own name, and a subcommand is a command.
        let commands = [
            section("commander", "Commander, the complete solution for command-line programs."),
            section("Help.subcommandTerm", "`subcommandTerm(cmd: Command): string`\n\nThe term commander shows a subcommand by."),
            section("Commands", "`.command()` adds a command; `{ isDefault: true }` runs it when no other is named."),
            section("Option", "An option commander parses."),
        ];
        let (told, _) = select(&commands, "commander", "add a farewell subcommand with commander");
        assert!(told.contains("### Commands") && !told.contains("### Option"), "{told}");
        // A cut code block is closed.
        assert_eq!(cut("```\nlet a = 1;\nlet b = 2;\n```", 16), "```\nlet a = 1;\n```\n…");
    }
}

