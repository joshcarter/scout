//! The one read-side row.
//!
//! `dashboard` renders rows and `live` synthesizes them for calls that have not
//! reached the log yet.  Both used to declare the same twenty fields — `live`
//! said so out loud ("mirrors `dashboard::Row` without creating a module
//! cycle") — and `dashboard` carried a hand-written field-by-field converter
//! between the two copies.  The cycle was real; copying the type was the wrong
//! way out of it.  A leaf module both sides depend on breaks it properly, and
//! now a field added here is a field both sides have.
//!
//! This is deliberately *not* `stats::CallRecord`.  That type is the write-side
//! builder — chained setters, `silent`, `Ledger` integration, `to_json` for
//! `calls.jsonl` — and it holds a typed `Outcome` and `Option<u64>` byte counts
//! because it is still being assembled.  A `Row` is what a reader has after the
//! fact: everything already decided, the outcome flattened to the string that
//! crossed the wire.  Merging them would mean one type that is half-built in
//! half its uses.

use crate::stats::Outcome;
use serde_json::{json, Value};

/// One call, as a reader sees it: either parsed from a `"v":2` log line or
/// synthesized from the live channel for a call still in flight.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    /// The operation this row belongs to — the grouping key, stamped by the
    /// writer's ledger.  A row from before `op` was recorded falls back to its
    /// own `id`, which makes it an operation of one; see `group_ops`.
    pub op: String,
    pub run: String,
    pub ts: f64,
    pub via: String,
    pub tool: String,
    pub preset: String,
    pub attempt: u64,
    pub project: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub input: Value,
    /// `Outcome::as_str`, or `live::ABANDONED`, or `"running"` — the last two
    /// are daemon-synthesized states no `Outcome` has, which is why this stays
    /// a string rather than becoming the enum.
    pub kind: String,
    pub summary: Option<String>,
    pub raw_bytes: u64,
    pub returned_bytes: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub ms: u64,
    pub ok: bool,
}

impl Row {
    /// Parse one log line, or `None` if it is not a current-schema record.
    ///
    /// Readers take only lines carrying `"v":2` — the shape that has `id`,
    /// `op`, `via` and `input`.  Older lines are skipped rather than padded out
    /// with synthesized identity and an empty `input`, which is what made a
    /// pre-`input` record indistinguishable in the UI from a call that
    /// genuinely had no arguments.  They stay in the log; `scout stats` still
    /// counts them, and the dashboard simply has nothing to show for a row
    /// whose arguments, prompt and response were never recorded.
    pub fn parse(line: &str) -> Option<Row> {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("v").and_then(Value::as_u64) != Some(2) {
            return None;
        }
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let preset = s("preset").unwrap_or_else(|| "unknown".to_string());
        let id = s("id")?;
        let kind = v["outcome"]["kind"]
            .as_str()
            .map_or_else(|| if ok { "ok" } else { "unknown" }.to_string(), str::to_string);
        Some(Row {
            op: s("op").unwrap_or_else(|| id.clone()),
            run: s("run").unwrap_or_else(|| id.clone()),
            id,
            ts: v.get("ts").and_then(Value::as_f64).unwrap_or(0.0),
            via: s("via").unwrap_or_default(),
            tool: s("tool").unwrap_or_else(|| preset.clone()),
            preset,
            attempt: v.get("attempt").and_then(Value::as_u64).unwrap_or(1),
            project: s("project"),
            model: s("model"),
            endpoint: s("endpoint"),
            input: v.get("input").cloned().unwrap_or_else(|| json!({})),
            kind,
            summary: v["outcome"]["summary"].as_str().map(str::to_string),
            raw_bytes: n("raw_bytes"),
            returned_bytes: n("returned_bytes"),
            tokens_in: n("tokens_in"),
            tokens_out: n("tokens_out"),
            ms: n("ms"),
            ok,
        })
    }

    pub fn bypassed(&self) -> bool {
        self.kind == Outcome::Bypassed.as_str()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "op": self.op,
            "run": self.run,
            "ts": self.ts,
            "via": self.via,
            "tool": self.tool,
            "preset": self.preset,
            "attempt": self.attempt,
            "project": self.project,
            "model": self.model,
            "endpoint": self.endpoint,
            "input": self.input,
            "kind": self.kind,
            "summary": self.summary,
            "raw_bytes": self.raw_bytes,
            "returned_bytes": self.returned_bytes,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "ms": self.ms,
            "ok": self.ok,
        })
    }
}
