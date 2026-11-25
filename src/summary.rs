//! What a task did, kept as it runs and summarized once it settles.
//!
//! A task is the user's prompt and everything it sets going: tool rounds,
//! files written, commands and checks, fix rounds. The agent's last reply
//! often says little more than "Done.", so once nothing more is coming the
//! record goes to the agent's `/v1/chat/summary`, and a plain summary built
//! here stands in when that can't answer.

use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{Value, json};

/// The end of each command's output kept for the summary, in characters.
const OUTPUT_KEPT: usize = 800;

/// How long the summary may take before the plain one stands in.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(30);

/// Files the plain summary names; the rest are counted.
const FILES_NAMED: usize = 4;

#[derive(Debug, Clone, PartialEq)]
struct FileChange {
    path: String,
    created: bool,
    added: usize,
    removed: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct Command {
    command: String,
    /// `None` when a signal or the user ended it.
    exit_code: Option<i32>,
    output: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Check {
    command: String,
    passed: bool,
}

/// What one task did so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskRecord {
    request: String,
    reply: String,
    files: Vec<FileChange>,
    commands: Vec<Command>,
    checks: Vec<Check>,
}

impl TaskRecord {
    pub fn new(request: &str) -> Self {
        TaskRecord { request: request.to_string(), ..Self::default() }
    }

    /// A file written: a second write of the same file adds to the first,
    /// and a file the task made stays made.
    pub fn file(&mut self, path: &str, created: bool, added: usize, removed: usize) {
        match self.files.iter_mut().find(|f| f.path == path) {
            Some(file) => {
                file.added += added;
                file.removed += removed;
            }
            None => self.files.push(FileChange { path: path.to_string(), created, added, removed }),
        }
    }

    /// A command that ended, with the end of what it printed.
    pub fn command(&mut self, command: &str, exit_code: Option<i32>, output: &str) {
        self.commands.push(Command { command: command.to_string(), exit_code, output: tail(output.trim(), OUTPUT_KEPT) });
    }

    /// A check of the result: only the latest of each counts, as fix rounds
    /// run it again.
    pub fn check(&mut self, command: &str, passed: bool) {
        self.checks.retain(|c| c.command != command);
        self.checks.push(Check { command: command.to_string(), passed });
    }

    /// The agent's latest reply, its prose without code.
    pub fn reply(&mut self, prose: &str) {
        self.reply = prose.trim().to_string();
    }

    /// Whether the task is done work worth summing up: it changed files or
    /// checked them, and isn't waiting on a question to the user. Answers,
    /// lookups and questions are their own summary.
    pub fn worth_summarizing(&self) -> bool {
        let changed = !self.files.is_empty() || !self.checks.is_empty();
        changed && !self.reply.ends_with('?')
    }

    /// The `/v1/chat/summary` request for it.
    pub fn body(&self, project: &str) -> Value {
        json!({
            "project_id": project,
            "request": self.request,
            "reply": self.reply,
            "files": self.files.iter().map(|f| json!({
                "path": f.path,
                "change": if f.created { "created" } else { "updated" },
                "added": f.added,
                "removed": f.removed,
            })).collect::<Vec<_>>(),
            "commands": self.commands.iter().map(|c| json!({
                "command": c.command,
                "exit_code": c.exit_code,
                "output": c.output,
            })).collect::<Vec<_>>(),
            "checks": self.checks.iter().map(|c| json!({"command": c.command, "passed": c.passed})).collect::<Vec<_>>(),
        })
    }

    /// A summary from the record alone, for when the agent can't write one:
    /// "Created src/a.rs (+12 −0). Ran 2 commands. `cargo test` passed."
    pub fn plain(&self) -> String {
        let mut sentences = Vec::new();
        if !self.files.is_empty() {
            let mut named: Vec<String> = self
                .files
                .iter()
                .take(FILES_NAMED)
                .map(|f| format!("{} {} (+{} −{})", if f.created { "created" } else { "updated" }, f.path, f.added, f.removed))
                .collect();
            if self.files.len() > FILES_NAMED {
                named.push(format!("{} more", self.files.len() - FILES_NAMED));
            }
            sentences.push(capitalized(&named.join(", ")));
        }
        if !self.commands.is_empty() {
            let failed: Vec<&str> = self
                .commands
                .iter()
                .filter(|c| c.exit_code != Some(0))
                .map(|c| c.command.lines().next().unwrap_or_default())
                .collect();
            let ran = match self.commands.len() {
                1 => "Ran 1 command".to_string(),
                n => format!("Ran {} commands", n),
            };
            sentences.push(match failed.as_slice() {
                [] => ran,
                [one] => format!("{}; `{}` failed", ran, one),
                more => format!("{}; {} failed", ran, more.len()),
            });
        }
        for check in &self.checks {
            sentences.push(format!("`{}` {}", check.command, if check.passed { "passed" } else { "failed" }));
        }
        let mut out = sentences.join(". ");
        if !out.is_empty() {
            out.push('.');
        }
        out
    }
}

/// The agent's summary of the task in `body`, asked of the agent at `api`
/// (its `/v1` URL): `None` when it has none to give, an error when it
/// can't be asked (an older agent has no such call).
pub fn summarize(client: &Client, api: &str, body: &Value) -> Result<Option<String>, String> {
    let response = client
        .post(format!("{}/chat/summary", api))
        .timeout(SUMMARY_TIMEOUT)
        .json(body)
        .send()
        .map_err(|e| format!("Failed to summarize: {}", e))?;
    if !response.status().is_success() {
        return Err(format!("Failed to summarize: {}", response.status()));
    }
    let reply: Value = response.json().map_err(|e| format!("Failed to summarize: {}", e))?;
    Ok(reply["summary"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The last `max` characters of `text`, marked when cut.
fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let at = text.char_indices().nth(count - max).map_or(0, |(i, _)| i);
    format!("…{}", &text[at..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worked() -> TaskRecord {
        let mut task = TaskRecord::new("add a --verbose flag");
        task.file("src/main.rs", false, 10, 2);
        task.command("cargo build", Some(0), "Finished");
        task.check("cargo test", false);
        task.file("src/main.rs", false, 2, 1);
        task.check("cargo test", true);
        task.reply("Done.");
        task
    }

    #[test]
    fn writes_and_checks_of_the_same_file_add_up() {
        let task = worked();
        assert_eq!(task.files, vec![FileChange { path: "src/main.rs".into(), created: false, added: 12, removed: 3 }]);
        assert_eq!(task.checks, vec![Check { command: "cargo test".into(), passed: true }], "the latest run counts");
    }

    #[test]
    fn a_file_made_stays_made() {
        let mut task = TaskRecord::new("x");
        task.file("a.rs", true, 3, 0);
        task.file("a.rs", false, 1, 1);
        assert!(task.files[0].created);
    }

    /// The `/v1/chat/summary` request as smartloop's `TaskSummaryRequest`
    /// reads it: smartloop's tests parse the same file, so a field renamed
    /// on either side fails a test rather than every summary.
    const CONTRACT: &str = include_str!("../tests/fixtures/task-summary-request.json");

    #[test]
    fn the_body_keeps_to_the_summary_contract() {
        let mut task = worked();
        task.file("src/cli.rs", true, 40, 0);
        task.command("cargo run", None, "");
        let contract: Value = serde_json::from_str(CONTRACT).expect("the fixture is JSON");
        assert_eq!(task.body("p1"), contract);
    }

    /// A one-request HTTP server on a free port answering `status` with
    /// `body`: its `/v1` URL, and the request it got once answered.
    fn agent(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let served = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let (mut head, mut length) = (String::new(), 0);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = n.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();
            reader.get_mut().write_all(response.as_bytes()).unwrap();
            format!("{}{}", head.lines().next().unwrap_or_default(), String::from_utf8(sent).unwrap())
        });
        (url, served)
    }

    #[test]
    fn the_summary_is_asked_of_the_agent_and_read_from_its_answer() {
        let body = worked().body("p1");
        let ask = |status: &str, answer: &str| {
            let (url, served) = agent(status, answer);
            let got = summarize(&Client::new(), &url, &body);
            (got, served.join().unwrap())
        };
        let (got, request) = ask("200 OK", r#"{"summary": "  - Added a --verbose flag\n"}"#);
        assert_eq!(got, Ok(Some("- Added a --verbose flag".into())));
        assert!(request.starts_with("POST /v1/chat/summary HTTP/1.1"), "{}", request);
        let sent: Value = serde_json::from_str(&request[request.find('{').unwrap()..]).unwrap();
        assert_eq!(sent, body);
        // None to give: the plain summary stands in, with nothing wrong.
        assert_eq!(ask("200 OK", r#"{"summary": null}"#).0, Ok(None));
        assert_eq!(ask("200 OK", r#"{"summary": "  "}"#).0, Ok(None));
        // Not asked: why, for the user.
        assert_eq!(ask("404 Not Found", "").0, Err("Failed to summarize: 404 Not Found".into()));
        assert_eq!(ask("500 Internal Server Error", "{}").0, Err("Failed to summarize: 500 Internal Server Error".into()));
        assert!(ask("200 OK", "not json").0.is_err_and(|e| e.starts_with("Failed to summarize: ")));
    }

    #[test]
    fn the_body_is_what_the_agent_reads() {
        let body = worked().body("p1");
        assert_eq!(body["project_id"], "p1");
        assert_eq!(body["request"], "add a --verbose flag");
        assert_eq!(body["reply"], "Done.");
        assert_eq!(body["files"][0], json!({"path": "src/main.rs", "change": "updated", "added": 12, "removed": 3}));
        assert_eq!(body["commands"][0], json!({"command": "cargo build", "exit_code": 0, "output": "Finished"}));
        assert_eq!(body["checks"], json!([{"command": "cargo test", "passed": true}]));
    }

    #[test]
    fn only_the_end_of_an_output_is_kept() {
        let mut task = TaskRecord::new("x");
        task.command("make", Some(1), &format!("{}error: at the end", "x".repeat(OUTPUT_KEPT * 2)));
        let output = &task.commands[0].output;
        assert!(output.ends_with("error: at the end") && output.starts_with('…'));
        assert_eq!(output.chars().count(), OUTPUT_KEPT + 1);
    }

    #[test]
    fn only_finished_changes_are_worth_summarizing() {
        assert!(worked().worth_summarizing());

        let mut answer = TaskRecord::new("what does this do?");
        answer.reply("It parses flags.");
        assert!(!answer.worth_summarizing(), "a plain answer");

        let mut lookup = TaskRecord::new("add a CONTRIBUTING.md");
        lookup.command("grep -rn DEV- .", Some(0), "");
        assert!(!lookup.worth_summarizing(), "commands alone change nothing");

        let mut asking = worked();
        asking.reply("Which prefix format do you use?");
        assert!(!asking.worth_summarizing(), "it waits on the user");
    }

    #[test]
    fn the_plain_summary_says_what_changed_ran_and_passed() {
        assert_eq!(worked().plain(), "Updated src/main.rs (+12 −3). Ran 1 command. `cargo test` passed.");

        let mut task = TaskRecord::new("x");
        for i in 0..6 {
            task.file(&format!("f{i}.rs"), true, 1, 0);
        }
        task.command("ls", Some(0), "");
        task.command("cargo run\n--release", Some(101), "panic");
        assert_eq!(
            task.plain(),
            "Created f0.rs (+1 −0), created f1.rs (+1 −0), created f2.rs (+1 −0), created f3.rs (+1 −0), 2 more. \
             Ran 2 commands; `cargo run` failed."
        );
    }
}
