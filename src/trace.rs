//! Opt-in timing, to find where a turn's time goes. With `DOCENT_TRACE=1`
//! set (or a file path), events go as JSON lines to
//! `~/.smartloop/logs/docent-trace.jsonl`, beside the agent's own log;
//! unset, every call returns at once.
//!
//! A turn's events carry its id, `<session>:<n>`, one per request sent.
//! A prompt and the fix, retry and tool rounds that follow it make a chain,
//! which ends with a `turn.end` totalling where its time went, once the app
//! is idle again.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// A frame slower than this is logged on its own, not only counted.
const SLOW_FRAME: Duration = Duration::from_millis(50);
/// While idle, frame counts go out this often.
const IDLE_FLUSH: Duration = Duration::from_secs(10);
/// Upper bounds of the frame-time buckets, in ms; the last catches the rest.
const BUCKETS: [u64; 7] = [1, 4, 8, 16, 33, 50, 100];

/// Why the event loop woke, to count redraws by cause.
#[derive(Clone, Copy)]
pub enum Wake {
    Key,
    Mouse,
    MouseMoved,
    Channel,
    Tick,
}

/// Where a chain's time went besides the model, which `stream_end` counts.
#[derive(Clone, Copy)]
pub enum Part {
    Check,
    Commands,
}

#[derive(Default)]
struct Frames {
    count: u64,
    total: Duration,
    max: Duration,
    buckets: [u64; BUCKETS.len() + 1],
    wakes: [u64; 5],
}

impl Frames {
    /// The bucket bound under which `p` of the frames fall, in ms.
    fn percentile(&self, p: f64) -> u64 {
        let want = (self.count as f64 * p).ceil() as u64;
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= want {
                return BUCKETS.get(i).copied().unwrap_or(self.max.as_millis() as u64);
            }
        }
        self.max.as_millis() as u64
    }

    fn fields(&self) -> Value {
        json!({
            "count": self.count,
            "avg_ms": ms(self.total) / self.count.max(1) as f64,
            "p50_ms": self.percentile(0.5),
            "p95_ms": self.percentile(0.95),
            "max_ms": ms(self.max),
            "buckets_ms": BUCKETS
                .iter()
                .map(|b| format!("<{b}"))
                .chain(["more".to_string()])
                .zip(self.buckets.map(Value::from))
                .collect::<serde_json::Map<_, _>>(),
            "wakes": {
                "key": self.wakes[0],
                "mouse": self.wakes[1],
                "mouse_moved": self.wakes[2],
                "channel": self.wakes[3],
                "tick": self.wakes[4],
            },
        })
    }
}

/// The content chunks of one request, as the app took them in.
#[derive(Default)]
struct Chunks {
    count: u64,
    total: Duration,
    max: Duration,
    write_done: Duration,
}

/// A prompt and the rounds that follow it.
struct Chain {
    started: Instant,
    rounds: u32,
    model: Duration,
    check: Duration,
    commands: Duration,
    context_chars: usize,
    context_ms: f64,
    prefix_shared: Option<f64>,
    first_token: Option<Duration>,
    tokens: u64,
    generating: Duration,
    file_bytes: usize,
    lines_changed: usize,
}

struct State {
    out: BufWriter<File>,
    session: String,
    turn: Option<String>,
    turns: u64,
    chain: Option<Chain>,
    frames: Frames,
    frames_since: Instant,
    chunks: Chunks,
    /// The last system message sent, to measure how much of the next one
    /// starts the same, as much as the model's prompt cache could reuse.
    last_system: String,
}

static STATE: OnceLock<Option<Mutex<State>>> = OnceLock::new();

fn state() -> Option<&'static Mutex<State>> {
    STATE.get_or_init(open).as_ref()
}

/// Where `DOCENT_TRACE` says to write, if anywhere.
fn path() -> Option<PathBuf> {
    let set = std::env::var("DOCENT_TRACE").ok()?;
    match set.trim() {
        "" | "0" | "false" | "off" => None,
        "1" | "true" | "on" => Some(crate::framework::install_dir().join("logs").join("docent-trace.jsonl")),
        file => Some(PathBuf::from(file)),
    }
}

fn open() -> Option<Mutex<State>> {
    if cfg!(test) {
        return None;
    }
    let path = path()?;
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
    Some(Mutex::new(State {
        out: BufWriter::new(file),
        session: String::new(),
        turn: None,
        turns: 0,
        chain: None,
        frames: Frames::default(),
        frames_since: Instant::now(),
        chunks: Chunks::default(),
        last_system: String::new(),
    }))
}

/// Whether tracing is on.
pub fn on() -> bool {
    state().is_some()
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let mut state = state()?.lock().unwrap_or_else(|e| e.into_inner());
    Some(f(&mut state))
}

pub fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e6).round() / 1e3
}

/// One event as its JSON line: the time, in µs since the epoch, so it lines
/// up with other logs, the session and turn, then `fields`.
fn line(ts: u128, session: &str, turn: Option<&str>, ev: &str, fields: Value) -> String {
    let mut event = json!({"ts": ts as u64, "session": session, "turn": turn, "ev": ev});
    if let (Some(event), Value::Object(fields)) = (event.as_object_mut(), fields) {
        event.extend(fields);
    }
    event.to_string()
}

impl State {
    fn emit(&mut self, ev: &str, fields: Value) {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros());
        let line = line(ts, &self.session, self.turn.as_deref(), ev, fields);
        let _ = writeln!(self.out, "{line}");
        let _ = self.out.flush();
    }

    fn flush_frames(&mut self) {
        if self.frames.count > 0 {
            let fields = self.frames.fields();
            self.emit("ui.frames", fields);
        }
        self.frames = Frames::default();
        self.frames_since = Instant::now();
    }
}

/// Write `ev` with `fields`, as part of the current turn.
pub fn event(ev: &str, fields: Value) {
    with(|s| s.emit(ev, fields));
}

/// The session starts.
pub fn start(session: &str, fields: Value) {
    with(|s| {
        s.session = session.to_string();
        s.emit("session.start", fields);
    });
}

/// A request of `session` is about to be built: a prompt of the user's,
/// which starts a chain unless one is going, or a round of one (`kind`).
pub fn turn_begin(session: &str, kind: &str, making: bool, prompt_chars: usize) {
    with(|s| {
        if s.session != session {
            s.session = session.to_string();
        }
        s.turns += 1;
        s.turn = Some(format!("{}:{}", s.session, s.turns));
        s.chunks = Chunks::default();
        let chain = s.chain.get_or_insert_with(|| Chain {
            started: Instant::now(),
            rounds: 0,
            model: Duration::ZERO,
            check: Duration::ZERO,
            commands: Duration::ZERO,
            context_chars: 0,
            context_ms: 0.0,
            prefix_shared: None,
            first_token: None,
            tokens: 0,
            generating: Duration::ZERO,
            file_bytes: 0,
            lines_changed: 0,
        });
        chain.rounds += 1;
        s.emit("turn.begin", json!({"kind": kind, "making": making, "prompt_chars": prompt_chars}));
    });
}

/// The project context was built, taking `took`.
pub fn context_built(took: Duration, chars: usize, fields: Value) {
    with(|s| {
        if let Some(chain) = &mut s.chain {
            chain.context_chars = chars;
            chain.context_ms += ms(took);
        }
        s.emit("context.build", fields);
    });
}

/// The bytes `a` and `b` start with alike, cut back to a char boundary.
pub fn shared_prefix(a: &str, b: &str) -> usize {
    let n = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    (0..=n).rev().find(|i| a.is_char_boundary(*i)).unwrap_or(0)
}

/// The system message about to go: how much of it starts as the last
/// one did.
pub fn system_prompt(system: &str) {
    with(|s| {
        let shared = shared_prefix(&s.last_system, system);
        let fraction = shared as f64 / system.len().max(1) as f64;
        if let Some(chain) = &mut s.chain {
            chain.prefix_shared = Some(fraction);
        }
        s.emit(
            "context.prefix",
            json!({"system_chars": system.len(), "shared_prefix_chars": shared, "shared_fraction": (fraction * 1000.0).round() / 1000.0}),
        );
        s.last_system = system.to_string();
    });
}

/// The first content of the request came, `after` it was sent.
pub fn first_token(after: Duration) {
    with(|s| {
        if let Some(chain) = &mut s.chain {
            chain.first_token.get_or_insert(after);
        }
        s.emit("first_token", json!({"ms": ms(after)}));
    });
}

/// The request's stream ended, `took` after it was sent.
pub fn stream_end(took: Duration, first: Option<Duration>, tokens: u64, bytes: usize, finish: Option<&str>) {
    with(|s| {
        let generating = took.saturating_sub(first.unwrap_or(took));
        if let Some(chain) = &mut s.chain {
            chain.model += took;
            chain.tokens += tokens;
            chain.generating += generating;
        }
        let rate = tokens as f64 / generating.as_secs_f64().max(1e-3);
        s.emit(
            "stream.end",
            json!({
                "ms": ms(took),
                "gen_ms": ms(generating),
                "tokens": tokens,
                "content_bytes": bytes,
                "tok_per_s": (rate * 10.0).round() / 10.0,
                "finish_reason": finish,
            }),
        );
    });
}

/// The reply's files were written: `bytes` given for `lines` changed.
pub fn reply_files(bytes: usize, lines: usize, files: Vec<Value>) {
    with(|s| {
        if let Some(chain) = &mut s.chain {
            chain.file_bytes += bytes;
            chain.lines_changed += lines;
        }
        s.emit("reply.files", json!({"bytes": bytes, "lines_changed": lines, "files": files}));
    });
}

/// `took` went to `part` of the chain, with `ev` and `fields` said of it.
pub fn spent(part: Part, took: Duration, ev: &str, mut fields: Value) {
    with(|s| {
        if let Some(chain) = &mut s.chain {
            match part {
                Part::Check => chain.check += took,
                Part::Commands => chain.commands += took,
            }
        }
        fields["ms"] = json!(ms(took));
        s.emit(ev, fields);
    });
}

/// A content chunk took `took` to take in, `write_done` of it writing
/// files whose blocks were done.
pub fn chunk(took: Duration, write_done: Duration) {
    with(|s| {
        let c = &mut s.chunks;
        c.count += 1;
        c.total += took;
        c.max = c.max.max(took);
        c.write_done += write_done;
    });
}

/// The request's reply is in: what its chunks cost the app.
pub fn turn_done() {
    with(|s| {
        let c = std::mem::take(&mut s.chunks);
        if c.count > 0 {
            s.emit(
                "ui.chunks",
                json!({"count": c.count, "total_ms": ms(c.total), "max_ms": ms(c.max), "write_done_ms": ms(c.write_done)}),
            );
        }
    });
}

/// The event loop woke for `why`.
pub fn wake(why: Wake) {
    with(|s| s.frames.wakes[why as usize] += 1);
}

/// A frame took `took` to draw, with `entries` in the chat.
pub fn frame(took: Duration, entries: usize) {
    with(|s| {
        let f = &mut s.frames;
        f.count += 1;
        f.total += took;
        f.max = f.max.max(took);
        let at = BUCKETS.iter().position(|b| took < Duration::from_millis(*b)).unwrap_or(BUCKETS.len());
        f.buckets[at] += 1;
        if took >= SLOW_FRAME {
            s.emit("ui.slow_frame", json!({"ms": ms(took), "entries": entries}));
        }
        if s.chain.is_none() && s.frames_since.elapsed() >= IDLE_FLUSH {
            s.flush_frames();
        }
    });
}

/// Whether a chain is going, to end once the app is idle.
pub fn chained() -> bool {
    with(|s| s.chain.is_some()).unwrap_or(false)
}

/// The app is idle again: the chain ends, totalled, and a line of it comes
/// back to show.
pub fn settled() -> Option<String> {
    with(|s| {
        let chain = s.chain.take()?;
        let p95 = (s.frames.count > 0).then(|| s.frames.percentile(0.95));
        let total = chain.started.elapsed();
        let rate = chain.tokens as f64 / chain.generating.as_secs_f64().max(1e-3);
        s.emit(
            "turn.end",
            json!({
                "ms": ms(total),
                "rounds": chain.rounds,
                "model_ms": ms(chain.model),
                "check_ms": ms(chain.check),
                "commands_ms": ms(chain.commands),
                "context_ms": chain.context_ms,
                "context_chars": chain.context_chars,
                "prefix_shared": chain.prefix_shared,
                "first_token_ms": chain.first_token.map(ms),
                "tokens": chain.tokens,
                "tok_per_s": (rate * 10.0).round() / 10.0,
                "file_bytes": chain.file_bytes,
                "lines_changed": chain.lines_changed,
                "frames_p95_ms": p95,
            }),
        );
        s.flush_frames();
        let mut parts = vec![format!("{} in {} round(s)", crate::progress::format_duration(total), chain.rounds)];
        if chain.context_chars > 0 {
            let reused = chain.prefix_shared.map_or(String::new(), |f| format!(", {:.0}% prefix reused", f * 100.0));
            parts.push(format!("ctx {}k chars ({:.0} ms{})", chain.context_chars / 1000, chain.context_ms, reused));
        }
        if let Some(first) = chain.first_token {
            parts.push(format!("first token {:.1}s", first.as_secs_f64()));
        }
        parts.push(format!("{} tok @ {:.0} tok/s", chain.tokens, rate));
        if chain.file_bytes > 0 {
            parts.push(format!("files {} for {} changed lines", crate::progress::format_size(chain.file_bytes as u64), chain.lines_changed));
        }
        if chain.check > Duration::ZERO {
            parts.push(format!("checks {:.1}s", chain.check.as_secs_f64()));
        }
        if chain.commands > Duration::ZERO {
            parts.push(format!("commands {:.1}s", chain.commands.as_secs_f64()));
        }
        if let Some(p95) = p95 {
            parts.push(format!("frames p95 {p95} ms"));
        }
        Some(format!("trace: {}", parts.join(" · ")))
    })
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_in_tests() {
        assert!(!on());
        event("x", json!({}));
        assert!(settled().is_none());
    }

    #[test]
    fn line_has_its_keys_then_fields() {
        let v: Value = serde_json::from_str(&line(12, "s", Some("s:1"), "ev", json!({"ms": 1.5}))).unwrap();
        assert_eq!(v["ts"], 12);
        assert_eq!(v["session"], "s");
        assert_eq!(v["turn"], "s:1");
        assert_eq!(v["ev"], "ev");
        assert_eq!(v["ms"], 1.5);
        let v: Value = serde_json::from_str(&line(0, "s", None, "ev", json!({}))).unwrap();
        assert!(v["turn"].is_null());
    }

    #[test]
    fn shared_prefix_stops_at_a_char() {
        assert_eq!(shared_prefix("", "abc"), 0);
        assert_eq!(shared_prefix("abcd", "abxd"), 2);
        assert_eq!(shared_prefix("abc", "abc"), 3);
        // "é" is two bytes alike in its first only.
        assert_eq!(shared_prefix("aé", "aè"), 1);
    }

    #[test]
    fn percentile_by_bucket() {
        let mut f = Frames::default();
        for ms in [0, 0, 0, 2, 20] {
            f.count += 1;
            f.max = f.max.max(Duration::from_millis(ms));
            let at = BUCKETS.iter().position(|b| Duration::from_millis(ms) < Duration::from_millis(*b)).unwrap();
            f.buckets[at] += 1;
        }
        assert_eq!(f.percentile(0.5), 1);
        assert_eq!(f.percentile(0.95), 33);
    }
}
