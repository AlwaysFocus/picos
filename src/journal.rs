//! Public experiment evidence. Private reasoning and raw provider output are excluded.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs::OpenOptions, io::Write, path::Path};

#[derive(Default, Serialize, Deserialize)]
pub struct Journal {
    #[serde(default)]
    entries: Vec<Value>,
    #[serde(default)]
    turns: Vec<Value>,
    #[serde(default)]
    latest_id: u64,
}

fn public_type(kind: &str) -> bool {
    matches!(
        kind,
        "agent.message"
            | "agent.turn"
            | "agent.start"
            | "agent.completed"
            | "agent.stopped"
            | "agent.failed"
            | "agent.error"
            | "agent.limit"
            | "agent.session"
            | "agent.warning"
            | "agent.tool"
            | "tool"
            | "tool-error"
            | "assemble"
            | "system"
            | "error"
            | "operator.turns"
            | "operator.settings"
            | "operator.continue"
            | "operator.stop"
    )
}
impl Journal {
    pub fn record(&mut self, mut event: Value, path: Option<&Path>) {
        if !public_type(event["type"].as_str().unwrap_or("")) {
            return;
        }
        self.latest_id = self.latest_id.saturating_add(1);
        event["id"] = json!(self.latest_id);
        if let Some(path) = path {
            Self::append(path, &event);
        }
        self.entries.push(event);
        self.trim();
    }
    pub fn prompt(&mut self, mut turn: Value, path: &Path) {
        self.latest_id = self.latest_id.saturating_add(1);
        turn["id"] = json!(self.latest_id);
        turn["type"] = json!("agent.prompt");
        Self::append(path, &turn);
        self.turns.push(turn);
        if self.turns.len() > 24 {
            self.turns.remove(0);
        }
    }
    fn append(path: &Path, event: &Value) {
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new().create(true).append(true).open(path)?;
            writeln!(file, "{event}")
        })();
        if let Err(error) = result {
            eprintln!("Public journal write failed: {error}");
        }
    }
    pub fn restore_bounds(&mut self) {
        self.entries
            .retain(|v| public_type(v["type"].as_str().unwrap_or("")));
        self.turns.retain(|v| {
            v["type"] == "agent.prompt" && v["prompt"].as_str().is_some_and(|s| s.len() <= 65536)
        });
        if self.turns.len() > 24 {
            self.turns.drain(..self.turns.len() - 24);
        }
        self.trim();
    }
    fn trim(&mut self) {
        if self.entries.len() > 400 {
            self.entries.drain(..self.entries.len() - 400);
        }
        // Keep full public messages, dropping older entries rather than silently clipping them.
        let mut bytes = self
            .entries
            .iter()
            .map(|v| v.to_string().len())
            .sum::<usize>();
        while bytes > 2 * 1024 * 1024 && self.entries.len() > 1 {
            bytes -= self.entries.remove(0).to_string().len();
        }
    }
    pub fn view(&self, after: u64) -> Value {
        json!({"entries":self.entries.iter().filter(|v|v["id"].as_u64().unwrap_or(0)>after).collect::<Vec<_>>(),
            "turns":self.turns,"oldestId":self.entries.first().and_then(|v|v["id"].as_u64()).unwrap_or(0),"latestId":self.latest_id,
            "retention":"Most recent 400 public entries (up to 2 MiB) and 24 submitted prompts. Full new records are stored in journal.jsonl."})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_public_evidence_is_retained_and_cursors_are_monotonic() {
        let mut journal = Journal::default();
        for kind in ["agent.reasoning", "agent.output", "analysis"] {
            journal.record(json!({"type":kind,"message":"hidden"}), None);
        }
        assert_eq!(journal.view(0)["latestId"], 0);
        for i in 0..405 {
            journal.record(
                json!({"type":"agent.message","message":format!("public {i}"),"turn":1}),
                None,
            );
        }
        let view = journal.view(403);
        assert_eq!(view["oldestId"], 6);
        assert_eq!(view["entries"].as_array().unwrap().len(), 2);
        assert_eq!(view["entries"][1]["id"], 405);
        let saved = serde_json::to_value(&journal).unwrap();
        let restored: Journal = serde_json::from_value(saved).unwrap();
        assert_eq!(restored.view(0), journal.view(0));
    }
}
