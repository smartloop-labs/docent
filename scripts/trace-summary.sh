#!/usr/bin/env bash
# Sums up a DOCENT_TRACE log: one row per prompt (a chain of rounds), with
# where its time went, then the frames drawn meanwhile. Reads the default
# log, or FILE if given; SESSION picks one session's prompts.
#
#   scripts/trace-summary.sh
#   scripts/trace-summary.sh ~/.smartloop/logs/docent-trace.jsonl <session>
set -euo pipefail

FILE=${1:-${SLP_HOME:-$HOME/.smartloop}/logs/docent-trace.jsonl}
SESSION=${2:-}

jq -rs --arg session "$SESSION" '
  def r: if . == null then "-" else (. | tostring) end;
  def s: if . == null then "-" else ((. / 1000 * 10 | round) / 10 | tostring) + "s" end;
  map(select($session == "" or .session == $session))
  | (["ended", "rounds", "total", "ctx chars", "ctx", "prefix", "1st tok", "model", "tok", "tok/s",
      "file B", "lines", "checks", "cmds", "frame p95"] | @tsv),
    (.[] | select(.ev == "turn.end")
      | [(.ts / 1e6 | floor | strflocaltime("%H:%M:%S")), .rounds, (.ms | s), .context_chars,
         (.context_ms | round | tostring) + "ms", (if .prefix_shared == null then "-" else ((.prefix_shared * 100 | round | tostring) + "%") end),
         (.first_token_ms | s), (.model_ms | s), .tokens, .tok_per_s, .file_bytes, .lines_changed,
         (.check_ms | s), (.commands_ms | s), (.frames_p95_ms | r)]
      | map(r) | @tsv)
' "$FILE" | column -t -s $'\t'

echo
echo "Rounds:"
jq -rs --arg session "$SESSION" '
  map(select($session == "" or .session == $session))
  | group_by(.turn) | map(select(.[0].turn != null))
  | .[] | {
      turn: .[0].turn,
      kind: (map(select(.ev == "turn.begin"))[0].kind),
      ui: (map(select(.ev == "turn.dispatch"))[0].ui_ms),
      status: (map(select(.ev == "server.status") | "\(.step)@\(.ms | floor)ms") | join(" ")),
      first: (map(select(.ev == "first_token"))[0].ms),
      end: (map(select(.ev == "stream.end"))[0]),
      chunks: (map(select(.ev == "ui.chunks"))[0]),
    }
  | "\(.turn)  \(.kind)  ui \(.ui // "-")ms  first \(.first // "-")ms  \(.end.tokens // "-") tok @ \(.end.tok_per_s // "-")/s  chunks \(.chunks.total_ms // "-")ms (write \(.chunks.write_done_ms // "-")ms)  \(.status)"
' "$FILE"
