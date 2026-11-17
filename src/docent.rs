//! The project's `.docent/` folder: which Smartloop project a folder
//! belongs to, and the skills it hands on to that project.
//!
//! `.docent/manifest.json` records the project the folder is linked to and
//! every skill pushed to it: its hash, and the id it has there. Documents
//! aren't pushed: the agent reads the code itself with `explore`.
//!
//! The project root is the nearest folder up from the working directory that
//! holds a `.docent/`, or the working directory when none does. Each machine
//! also records the folder every project is linked to, in
//! `~/.smartloop/docent-folders.json`, so a project keeps its own folder
//! whichever one the chat was opened in. A project has one folder: linking
//! it to another leaves the old one.
//!
//! Skills are `.docent/skills/<name>.md` or `.docent/skills/<name>/SKILL.md`;
//! they are the part of `.docent/` meant to be committed, the manifest is
//! local. `push_skills` brings the project's skills in line with them, when
//! the chat opens the project and after a reply writes one.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{error_message, projects_url, request_json};

pub const DIR: &str = ".docent";
const MANIFEST: &str = "manifest.json";
const SKILLS: &str = "skills";
/// The folder each project is linked to on this machine, in the install dir.
const FOLDERS: &str = "docent-folders.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Entry {
    hash: String,
    /// The skill's id in the project.
    remote: String,
    synced_at: u64,
}

/// What a folder holds for the agent: code, so a prompt there is answered
/// with code that fits it, or documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Code,
    Documents,
}

/// What an earlier version recorded beyond these (whether its sync was
/// paused) is dropped on the next save; the documents it pushed are read,
/// for `retire_documents` to delete, and never written back.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    project: Option<String>,
    /// A mark made by hand in an earlier version; none reads it off the files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<Kind>,
    /// Keyed by path relative to `.docent/`.
    #[serde(default)]
    skills: BTreeMap<String, Entry>,
    /// The documents an earlier version's sync pushed, by their name in
    /// the project: what's left of them to delete.
    #[serde(default, rename = "documents", skip_serializing_if = "BTreeMap::is_empty")]
    pushed: BTreeMap<String, Uploaded>,
}

/// A document an earlier version's sync pushed: the name the project
/// stores it under.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Uploaded {
    remote: String,
}

impl Manifest {
    fn load(root: &Path) -> Result<Self, String> {
        let path = root.join(DIR).join(MANIFEST);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} is not valid ({}); delete it to start over", path.display(), e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("Failed to read {}: {}", path.display(), e)),
        }
    }

    /// Written through a temporary file so a crash never leaves it half done.
    fn save(&self, root: &Path) -> Result<(), String> {
        let dir = root.join(DIR);
        fs::create_dir_all(&dir).map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;
        // The manifest holds this machine's project id: keep it out of git.
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            let _ = fs::write(&ignore, format!("{}\n", MANIFEST));
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!("{}.tmp", MANIFEST));
        fs::write(&tmp, text + "\n").map_err(|e| format!("Failed to write {}: {}", tmp.display(), e))?;
        fs::rename(&tmp, dir.join(MANIFEST)).map_err(|e| format!("Failed to save the manifest: {}", e))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    Add,
    Update,
    Remove,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

fn relative_key(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn hash(content: &str) -> String {
    Sha256::digest(content.as_bytes()).iter().map(|b| format!("{:02x}", b)).collect()
}

/// The project's skills, by path relative to `.docent/`, with their content.
fn scan_skills(root: &Path) -> BTreeMap<String, String> {
    let base = root.join(DIR);
    let mut files = BTreeMap::new();
    let Ok(entries) = fs::read_dir(base.join(SKILLS)) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let skill = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.extension().is_some_and(|e| e == "md") {
            path
        } else {
            continue;
        };
        if let Ok(content) = fs::read_to_string(&skill) {
            files.insert(relative_key(&base, &skill), content);
        }
    }
    files
}

/// What it takes to bring `known` to `found`.
fn diff(found: &BTreeMap<String, String>, known: &BTreeMap<String, Entry>) -> Vec<(String, Change)> {
    let mut changes = Vec::new();
    for (key, content) in found {
        match known.get(key) {
            None => changes.push((key.clone(), Change::Add)),
            Some(entry) if entry.hash != hash(content) => changes.push((key.clone(), Change::Update)),
            Some(_) => {}
        }
    }
    changes.extend(known.keys().filter(|key| !found.contains_key(*key)).map(|key| (key.clone(), Change::Remove)));
    changes
}

/// A frontmatter value of a skill (`name:`, `description:`).
fn frontmatter(content: &str, key: &str) -> Option<String> {
    let block = content.strip_prefix("---")?.split("\n---").next()?;
    block
        .lines()
        .find_map(|line| {
            let value = line.trim().strip_prefix(key)?.strip_prefix(':')?;
            Some(value.trim().trim_matches(['"', '\'']).to_string())
        })
        .filter(|value| !value.is_empty())
}

/// The skill's name: its frontmatter's, else its file's or folder's.
fn skill_name(key: &str, content: &str) -> String {
    frontmatter(content, "name").unwrap_or_else(|| {
        let path = Path::new(key);
        let named = if path.file_name().is_some_and(|n| n == "SKILL.md") { path.parent() } else { Some(path) };
        named
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| key.to_string())
    })
}

/// The project at Smartloop the folder's skills go to.
struct Remote<'a> {
    client: &'a Client,
    project: String,
}

impl Remote<'_> {
    fn url(&self, tail: &str) -> String {
        format!("{}/{}/{}", projects_url(), self.project, tail)
    }

    fn documents(&self) -> Result<Vec<Value>, String> {
        let reply = request_json(self.client, self.url("documents"), "list documents")?;
        Ok(reply["documents"].as_array().cloned().unwrap_or_default())
    }

    fn delete_document(&self, id: &str, name: &str) -> Result<(), String> {
        let action = format!("delete {}", name);
        let response = self
            .client
            .delete(self.url(&format!("documents/{}", id)))
            .send()
            .map_err(|e| format!("Failed to {}: {}", action, e))?;
        if response.status().is_success() || response.status() == 404 {
            Ok(())
        } else {
            Err(error_message(&action, response))
        }
    }

    fn skills(&self) -> Result<Vec<Value>, String> {
        let reply = request_json(self.client, self.url("skills"), "list skills")?;
        Ok(reply["skills"].as_array().cloned().unwrap_or_default())
    }

    /// Send a skill request and return the skill's id.
    fn send_skill(&self, request: reqwest::blocking::RequestBuilder, action: &str) -> Result<String, String> {
        let response = request.send().map_err(|e| format!("Failed to {}: {}", action, e))?;
        if !response.status().is_success() {
            return Err(error_message(action, response));
        }
        let reply: Value = response.json().map_err(|e| format!("Failed to {}: {}", action, e))?;
        Ok(reply["id"].as_str().unwrap_or_default().to_string())
    }

    fn add_skill(&self, name: &str, content: &str) -> Result<String, String> {
        let mut body = json!({ "content": content, "name": name });
        if let Some(description) = frontmatter(content, "description") {
            body["description"] = Value::String(description);
        }
        self.send_skill(self.client.post(self.url("skills")).json(&body), &format!("add skill {}", name))
    }

    fn update_skill(&self, id: &str, name: &str, content: &str) -> Result<String, String> {
        let request = self.client.patch(self.url(&format!("skills/{}", id))).json(&json!({ "content": content }));
        self.send_skill(request, &format!("update skill {}", name))?;
        Ok(id.to_string())
    }

    fn delete_skill(&self, id: &str, name: &str) -> Result<(), String> {
        let action = format!("delete skill {}", name);
        let response = self
            .client
            .delete(self.url(&format!("skills/{}", id)))
            .send()
            .map_err(|e| format!("Failed to {}: {}", action, e))?;
        if response.status().is_success() || response.status() == 404 {
            Ok(())
        } else {
            Err(error_message(&action, response))
        }
    }
}

/// Held while a manifest is written, so two pushes don't race.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `push_skills` did: `1 added, 1 updated`; empty when nothing
/// changed. Failures are listed apart, each with its reason.
#[derive(Debug, Default)]
pub struct Pushed {
    added: usize,
    updated: usize,
    removed: usize,
    pub failures: Vec<String>,
}

impl Pushed {
    pub fn line(&self) -> String {
        let parts: Vec<String> = [(self.added, "added"), (self.updated, "updated"), (self.removed, "removed")]
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, what)| format!("{} {}", n, what))
            .collect();
        parts.join(", ")
    }
}

/// Bring the skills of the project `root` is linked to in line with
/// `.docent/skills/`: add the new, update the changed, remove the deleted.
/// One deleted at Smartloop since is added again. Nothing when `root`
/// isn't linked.
pub fn push_skills(client: &Client, root: &Path) -> Result<Pushed, String> {
    let _held = lock();
    let mut manifest = Manifest::load(root)?;
    let Some(project) = manifest.project.clone() else { return Ok(Pushed::default()) };
    let found = scan_skills(root);
    if found.is_empty() && manifest.skills.is_empty() {
        return Ok(Pushed::default());
    }
    let remote = Remote { client, project };
    let ids: HashSet<String> = remote.skills()?.iter().filter_map(|s| s["id"].as_str().map(str::to_string)).collect();
    manifest.skills.retain(|_, e| ids.contains(&e.remote));

    let mut pushed = Pushed::default();
    for (key, change) in diff(&found, &manifest.skills) {
        let old = manifest.skills.get(&key).map(|e| e.remote.clone());
        let result = match change {
            Change::Remove => remote.delete_skill(old.as_deref().unwrap_or_default(), &key).map(|()| None),
            _ => {
                let content = &found[&key];
                let name = skill_name(&key, content);
                match &old {
                    Some(id) => remote.update_skill(id, &name, content),
                    None => remote.add_skill(&name, content),
                }
                .map(Some)
            }
        };
        match result {
            Ok(Some(id)) => {
                manifest.skills.insert(key.clone(), Entry { hash: hash(&found[&key]), remote: id, synced_at: now() });
                if change == Change::Add { pushed.added += 1 } else { pushed.updated += 1 }
            }
            Ok(None) => {
                manifest.skills.remove(&key);
                pushed.removed += 1;
            }
            Err(e) => pushed.failures.push(format!("{}: {}", key, e)),
        }
    }
    manifest.save(root)?;
    Ok(pushed)
}

/// The name a listed document is stored under in the project's inbox.
fn stored_name(document: &Value) -> Option<String> {
    let file = document["path"].as_str().and_then(|p| Path::new(p).file_name()?.to_str().map(str::to_string));
    file.or_else(|| document["filename"].as_str().map(str::to_string))
}

/// Delete from the project the documents an earlier version's sync pushed
/// from `root`, once: the agent reads the folder itself now, and each one
/// left indexed sends every request through a document search, a model
/// reading it again on every tool round. One the project lists without an
/// id isn't registered yet, and waits for the next time; one it no longer
/// has is forgotten. How many were deleted.
pub fn retire_documents(client: &Client, root: &Path) -> Result<usize, String> {
    let _held = lock();
    let mut manifest = Manifest::load(root)?;
    let Some(project) = manifest.project.clone() else { return Ok(0) };
    if manifest.pushed.is_empty() {
        return Ok(0);
    }
    let remote = Remote { client, project };
    let listed = remote.documents()?;
    let mut deleted = 0;
    let mut failure = None;
    let pushed = std::mem::take(&mut manifest.pushed);
    for (key, document) in pushed {
        let found = listed.iter().find(|d| stored_name(d).as_deref() == Some(document.remote.as_str()));
        match found.map(|d| d["id"].as_str()) {
            None => {}
            Some(None) => {
                manifest.pushed.insert(key, document);
            }
            Some(Some(id)) => match remote.delete_document(id, &document.remote) {
                Ok(()) => deleted += 1,
                Err(e) => {
                    failure = Some(e);
                    manifest.pushed.insert(key, document);
                }
            },
        }
    }
    manifest.save(root)?;
    match failure {
        Some(e) if deleted == 0 => Err(e),
        _ => Ok(deleted),
    }
}

/// Whether `path`, relative to `root`, is one of its skills.
pub fn is_skill(path: &str) -> bool {
    path.starts_with(&format!("{}/{}/", DIR, SKILLS)) && path.ends_with(".md")
}

/// Why `root` isn't taken as a project's folder: the home folder, or one
/// holding it, which holds everything a person owns.
pub fn refusal(root: &Path) -> Option<String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .and_then(|h| fs::canonicalize(h).ok());
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let holds_home = root.parent().is_none() || home.is_some_and(|home| home.starts_with(&root));
    holds_home.then(|| format!("{} holds your whole home folder; open docent in a project folder", root.display()))
}

/// The folder `start` is part of: the nearest one holding a `.docent/`, as
/// git finds its repository, else `start` itself. The home folder and those
/// above it are never taken.
pub fn root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .take_while(|dir| refusal(dir).is_none())
        .find(|dir| dir.join(DIR).is_dir())
        .map_or_else(|| start.to_path_buf(), Path::to_path_buf)
}

/// The Smartloop project `root` is linked to.
pub struct Link {
    pub project: String,
}

pub fn linked(root: &Path) -> Option<Link> {
    let manifest = Manifest::load(root).ok()?;
    Some(Link { project: manifest.project? })
}

/// The folder `project` is linked to on this machine; none when it was
/// never linked, or its folder has since been linked elsewhere, unlinked
/// or removed.
pub fn folder(project: &str) -> Option<PathBuf> {
    let root = Folders::load().0.remove(project)?;
    linked(&root).is_some_and(|l| l.project == project).then_some(root)
}

/// The folder a turn of `project` works in, from `here`: the project's own,
/// else the one `here` is part of unless that's linked to another project.
pub fn workspace(project: &str, here: &Path) -> Option<PathBuf> {
    folder(project).or_else(|| {
        let root = root(here);
        linked(&root).is_none_or(|l| l.project == project).then(|| here.to_path_buf())
    })
}

/// Each project's folder, by project id.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Folders(BTreeMap<String, PathBuf>);

impl Folders {
    fn path() -> PathBuf {
        // Each test runs on a thread of its own: keep theirs apart, and out
        // of the real install dir.
        if cfg!(test) {
            let name = format!("docent-folders-{}-{:?}.json", std::process::id(), std::thread::current().id());
            return std::env::temp_dir().join(name.replace(['(', ')'], ""));
        }
        crate::framework::install_dir().join(FOLDERS)
    }

    /// One unreadable starts afresh: the manifests still say what's linked.
    fn load() -> Self {
        fs::read_to_string(Self::path()).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text + "\n").map_err(|e| format!("Failed to write {}: {}", tmp.display(), e))?;
        fs::rename(&tmp, &path).map_err(|e| format!("Failed to save {}: {}", path.display(), e))
    }
}

/// Record `root` as `project`'s folder; a folder belongs to one project.
fn bind(root: &Path, project: &str) -> Result<(), String> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut folders = Folders::load();
    if folders.0.get(project) == Some(&root) {
        return Ok(());
    }
    folders.0.retain(|_, folder| *folder != root);
    folders.0.insert(project.to_string(), root);
    folders.save()
}

/// Link `root` to `project`; one linked elsewhere before starts its skills
/// afresh, and `project`'s folder before is its folder no more.
pub fn link(root: &Path, project: &str) -> Result<(), String> {
    let _held = lock();
    let mut manifest = Manifest::load(root)?;
    if manifest.project.as_deref() != Some(project) {
        manifest.skills.clear();
        manifest.project = Some(project.to_string());
    }
    manifest.save(root)?;
    bind(root, project)
}

/// What `root` was marked as holding, if it was.
pub fn kind(root: &Path) -> Option<Kind> {
    Manifest::load(root).ok()?.kind
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(content: &str) -> Entry {
        Entry { hash: hash(content), remote: "r".into(), synced_at: 0 }
    }

    #[test]
    fn linking_a_folder_marks_no_kind() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(kind(dir.path()), None);
        assert!(linked(dir.path()).is_none());
        link(dir.path(), "p1").unwrap();
        assert_eq!(kind(dir.path()), None);
        assert!(linked(dir.path()).is_some_and(|l| l.project == "p1"));
        let _ = fs::remove_file(Folders::path());
    }

    #[test]
    fn diff_finds_added_changed_and_removed() {
        let known = BTreeMap::from([("same".to_string(), entry("a")), ("edit".to_string(), entry("a")), ("gone".to_string(), entry("a"))]);
        let found = BTreeMap::from([
            ("same".to_string(), "a".to_string()),
            ("edit".to_string(), "b".to_string()),
            ("new".to_string(), "a".to_string()),
        ]);
        let mut changes = diff(&found, &known);
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            changes,
            vec![("edit".to_string(), Change::Update), ("gone".to_string(), Change::Remove), ("new".to_string(), Change::Add)]
        );
    }

    #[test]
    fn reads_skill_frontmatter() {
        let content = "---\nname: \"Summarize\"\ndescription: Short summaries\n---\nBody: text";
        assert_eq!(frontmatter(content, "name").as_deref(), Some("Summarize"));
        assert_eq!(frontmatter(content, "description").as_deref(), Some("Short summaries"));
        assert_eq!(frontmatter(content, "body"), None);
        assert_eq!(frontmatter("no frontmatter\nname: x", "name"), None);
    }

    #[test]
    fn names_skills_from_frontmatter_or_path() {
        assert_eq!(skill_name("skills/recap.md", "text"), "recap");
        assert_eq!(skill_name("skills/recap/SKILL.md", "text"), "recap");
        assert_eq!(skill_name("skills/recap.md", "---\nname: weekly\n---\n"), "weekly");
    }

    #[test]
    fn scans_only_skills() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (path, body) in [
            ("README.md", "readme"),
            (".docent/skills/recap.md", "---\nname: recap\n---\n"),
            (".docent/skills/weekly/SKILL.md", "weekly"),
            (".docent/skills/readme.txt", "not a skill"),
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        let skills = scan_skills(root);
        assert_eq!(skills.keys().collect::<Vec<_>>(), ["skills/recap.md", "skills/weekly/SKILL.md"]);
        assert_eq!(skills["skills/weekly/SKILL.md"], "weekly");
        assert!(is_skill(".docent/skills/weekly/SKILL.md") && is_skill(".docent/skills/recap.md"));
        assert!(!is_skill("skills/recap.md") && !is_skill(".docent/manifest.json"));
    }

    #[test]
    fn says_what_a_push_did() {
        assert_eq!(Pushed::default().line(), "");
        assert_eq!(Pushed { added: 1, ..Pushed::default() }.line(), "1 added");
        assert_eq!(Pushed { added: 2, removed: 1, ..Pushed::default() }.line(), "2 added, 1 removed");
    }

    #[test]
    fn home_and_root_are_never_taken() {
        let dir = tempfile::tempdir().unwrap();
        assert!(refusal(dir.path()).is_none());
        assert!(refusal(Path::new("/")).is_some());
        if let Some(home) = std::env::var_os("HOME") {
            assert!(refusal(Path::new(&home)).is_some());
        }
    }

    #[test]
    fn finds_the_root_above() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(root(&nested), nested);
        fs::create_dir(dir.path().join(DIR)).unwrap();
        assert_eq!(root(&nested), dir.path());
        assert_eq!(root(dir.path()), dir.path());
        fs::create_dir(nested.join(DIR)).unwrap();
        assert_eq!(root(&nested), nested);
    }

    #[test]
    fn a_project_has_the_one_folder_last_linked() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (fs::canonicalize(a.path()).unwrap(), fs::canonicalize(b.path()).unwrap());
        assert!(folder("p1").is_none());
        link(&a, "p1").unwrap();
        assert_eq!(folder("p1"), Some(a.clone()));
        link(&b, "p1").unwrap();
        assert_eq!(folder("p1"), Some(b.clone()));
        // A folder linked to another project leaves this one without.
        link(&b, "p2").unwrap();
        assert!(folder("p1").is_none());
        assert_eq!(folder("p2"), Some(b.clone()));
        drop(fs::remove_dir_all(&b));
        assert!(folder("p2").is_none());
        let _ = fs::remove_file(Folders::path());
    }

    #[test]
    fn a_turn_never_works_in_a_folder_linked_to_another_project() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (fs::canonicalize(a.path()).unwrap(), fs::canonicalize(b.path()).unwrap());
        let inside = a.join("docs");
        fs::create_dir_all(&inside).unwrap();
        assert_eq!(workspace("p1", &inside), Some(inside.clone()));
        link(&a, "p1").unwrap();
        assert_eq!(workspace("p1", &inside), Some(a.clone()));
        assert!(workspace("p2", &inside).is_none());
        link(&b, "p2").unwrap();
        assert_eq!(workspace("p2", &inside), Some(b.clone()));
        let _ = fs::remove_file(Folders::path());
    }

    #[test]
    fn an_old_manifest_loads_and_drops_its_documents() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(DIR)).unwrap();
        let old = r#"{"project": "p1", "paused": true, "documents": {"a.md": {"hash": "h", "size": 1, "mtime": 1, "remote": "a.md", "synced_at": 1}}, "skills": {"skills/x.md": {"hash": "h", "size": 1, "mtime": 1, "remote": "s1", "synced_at": 1}}}"#;
        fs::write(dir.path().join(DIR).join(MANIFEST), old).unwrap();
        let mut manifest = Manifest::load(dir.path()).unwrap();
        assert_eq!(manifest.project.as_deref(), Some("p1"));
        assert_eq!(manifest.skills["skills/x.md"].remote, "s1");
        // What the old sync pushed is kept until it's deleted from the project.
        assert_eq!(manifest.pushed["a.md"].remote, "a.md");
        manifest.save(dir.path()).unwrap();
        let saved = fs::read_to_string(dir.path().join(DIR).join(MANIFEST)).unwrap();
        assert!(saved.contains("\"documents\"") && !saved.contains("paused"), "{}", saved);
        assert!(!saved.contains("mtime\": 1, \"remote\": \"a.md"), "{}", saved);
        manifest.pushed.clear();
        manifest.save(dir.path()).unwrap();
        let saved = fs::read_to_string(dir.path().join(DIR).join(MANIFEST)).unwrap();
        assert!(!saved.contains("documents"), "{}", saved);
        assert_eq!(stored_name(&json!({"path": "/inbox/documents/README.md"})).as_deref(), Some("README.md"));
        assert_eq!(stored_name(&json!({"filename": "a.pdf"})).as_deref(), Some("a.pdf"));
        assert_eq!(fs::read_to_string(dir.path().join(DIR).join(".gitignore")).unwrap(), "manifest.json\n");
    }
}
