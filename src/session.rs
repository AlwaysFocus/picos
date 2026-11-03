//! Experiment orchestration. A model turn ending is not mission success.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::str::FromStr;

pub const DEFAULT_TURNS: u64 = 6;
/// Maximum size of one CLI allowance or browser grant.
pub const MAX_TURNS: u64 = 24;
pub const MAX_TOTAL_TURNS: u64 = 10_000;
pub const TOOLS_PER_TURN: u64 = 40;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunMode {
    #[default]
    Standard,
    ResourceLimit,
    Maximize,
}
impl FromStr for RunMode {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "standard" => Ok(Self::Standard),
            "resource-limit" => Ok(Self::ResourceLimit),
            "maximize" => Ok(Self::Maximize),
            _ => Err("Mode must be standard, resource-limit, or maximize.".into()),
        }
    }
}
impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::ResourceLimit => "resource-limit",
            Self::Maximize => "maximize",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub memory_revision: u64,
    pub cycles: u64,
    pub disk_touched_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "SavedSession")]
pub struct AgentSession {
    mission: String,
    pub model: Option<String>,
    turn: u64,
    max_turns: u64,
    mode: RunMode,
    tell_turns: bool,
    status: String,
    reason: String,
    stop_reason: Option<String>,
    baseline: Progress,
    stalled_turns: u64,
}

// Checkpoint data passes through validation before becoming executable policy.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedSession {
    mission: String,
    model: Option<String>,
    turn: u64,
    max_turns: u64,
    mode: RunMode,
    tell_turns: bool,
    status: String,
    reason: String,
    stop_reason: Option<String>,
    baseline: Progress,
    stalled_turns: u64,
}
impl TryFrom<SavedSession> for AgentSession {
    type Error = String;
    fn try_from(saved: SavedSession) -> Result<Self, Self::Error> {
        let session = Self {
            mission: saved.mission,
            model: saved.model,
            turn: saved.turn,
            max_turns: saved.max_turns,
            mode: saved.mode,
            tell_turns: saved.tell_turns,
            status: saved.status,
            reason: saved.reason,
            stop_reason: saved.stop_reason,
            baseline: saved.baseline,
            stalled_turns: saved.stalled_turns,
        };
        session.validate()?;
        Ok(session)
    }
}

impl AgentSession {
    pub fn new(mission: String, model: Option<String>, max_turns: u64) -> Result<Self, String> {
        Self::new_with_policy(mission, model, max_turns, RunMode::Standard, true)
    }
    pub fn new_with_policy(
        mission: String,
        model: Option<String>,
        max_turns: u64,
        mode: RunMode,
        tell_turns: bool,
    ) -> Result<Self, String> {
        if !(1..=MAX_TURNS).contains(&max_turns) {
            return Err(format!("Agent turns must be 1..{MAX_TURNS}."));
        }
        let session = Self {
            mission,
            model,
            turn: 0,
            max_turns,
            mode,
            tell_turns,
            status: "waiting".into(),
            reason: "Ready to begin the experiment.".into(),
            stop_reason: None,
            baseline: Progress::default(),
            stalled_turns: 0,
        };
        session.validate()?;
        Ok(session)
    }
    /// Migrate a saved machine that predates sessions without inventing unused grants.
    pub fn dormant(
        mission: String,
        model: Option<String>,
        mode: RunMode,
        tell_turns: bool,
    ) -> Result<Self, String> {
        let mut session = Self::new_with_policy(mission, model, DEFAULT_TURNS, mode, tell_turns)?;
        if mode == RunMode::Standard {
            session.max_turns = 0;
        }
        session.pause_on_restore();
        session.validate()?;
        Ok(session)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.mission.trim().is_empty() || self.mission.len() > 12_000 {
            return Err("Saved session mission must contain 1..12000 bytes.".into());
        }
        if self.model.as_ref().is_some_and(|model| {
            model.trim().is_empty() || model.len() > 120 || model.contains(['\r', '\n', '\0'])
        }) {
            return Err("Saved session model name is invalid.".into());
        }
        if self.max_turns > MAX_TOTAL_TURNS {
            return Err(format!(
                "Saved session turn allowance cannot exceed {MAX_TOTAL_TURNS}."
            ));
        }
        // The UI represents counters as exact JavaScript integers. Reserve one
        // increment for building the next prompt; resource mode has no turn budget.
        if self.turn >= 9_007_199_254_740_991 {
            return Err("Saved session iteration counter is too large.".into());
        }
        Ok(())
    }
    pub fn mission(&self) -> &str {
        &self.mission
    }
    pub fn mode(&self) -> RunMode {
        self.mode
    }
    pub fn tell_turns(&self) -> bool {
        self.tell_turns
    }
    /// A change affects future prompts; a previous disclosure cannot be undone.
    pub fn set_tell_turns(&mut self, tell_turns: bool) {
        self.tell_turns = tell_turns;
    }
    pub fn set_mode(&mut self, mode: RunMode) {
        // A legacy dormant standard session has no grants. Choosing maximize
        // supplies its normal initial allowance without changing consumption.
        if mode == RunMode::Maximize && self.max_turns == 0 && self.turn == 0 {
            self.max_turns = DEFAULT_TURNS;
        }
        self.mode = mode;
        if mode == RunMode::ResourceLimit
            && matches!(
                self.stop_reason.as_deref(),
                Some("turn_limit" | "tool_limit" | "stalled")
            )
        {
            self.stop("paused", "Mode changed; continue when ready.");
            self.stop_reason = Some("restored".into());
        }
    }
    pub fn can_continue(&self) -> bool {
        // Provider failures pause work, but do not consume the remaining grant.
        // Only an explicit operator request may start another turn after one.
        !matches!(self.status.as_str(), "running" | "waiting" | "stopping")
            && !matches!(
                self.stop_reason.as_deref(),
                Some("resource_limit" | "tool_limit")
            )
            && (self.mode == RunMode::ResourceLimit || self.turn < self.max_turns)
    }
    pub fn request_continue(&mut self) -> Result<(), String> {
        if !self.can_continue() {
            return Err(
                "This session cannot continue with its current allowance and stop reason.".into(),
            );
        }
        self.stop(
            "waiting",
            "Preparing another iteration against the same machine.",
        );
        self.stalled_turns = 0;
        Ok(())
    }
    pub fn tool_limit(&self) -> Option<u64> {
        (self.mode != RunMode::ResourceLimit).then(|| self.max_turns.saturating_mul(TOOLS_PER_TURN))
    }
    pub fn can_add_turns(&self) -> bool {
        self.mode == RunMode::Standard
            && self.max_turns.max(self.turn) < MAX_TOTAL_TURNS
            && (matches!(self.status.as_str(), "running" | "waiting")
                || matches!(
                    self.stop_reason.as_deref(),
                    Some("turn_limit" | "tool_limit" | "stalled" | "restored" | "stopped")
                )
                || (self.stop_reason.as_deref() == Some("failed") && self.turn >= self.max_turns))
    }
    /// Grant work on the same machine without refunding consumed tools or cycles.
    pub fn add_turns(&mut self, count: u64) -> Result<(), String> {
        if !(1..=MAX_TURNS).contains(&count) {
            return Err(format!("Grant 1..{MAX_TURNS} turns at a time."));
        }
        if !self.can_add_turns() {
            return Err("Grant turns to a working or paused standard session, or after a failed final turn. A failed session with turns remaining can be resumed without another grant.".into());
        }
        let total = self
            .max_turns
            .max(self.turn)
            .checked_add(count)
            .filter(|total| *total <= MAX_TOTAL_TURNS)
            .ok_or_else(|| format!("Session allowance cannot exceed {MAX_TOTAL_TURNS} turns."))?;
        self.max_turns = total;
        self.stalled_turns = 0;
        if self.status != "running" {
            self.status = "waiting".into();
            self.reason =
                "More turns granted; preparing the next iteration against the same machine.".into();
            self.stop_reason = None;
        }
        Ok(())
    }
    pub fn state(&self) -> Value {
        json!({"turn":self.turn,"maxTurns":if self.mode == RunMode::ResourceLimit {None} else {Some(self.max_turns)},
            "status":self.status,"reason":self.reason,"stopReason":self.stop_reason,"stalledTurns":self.stalled_turns,
            "mode":self.mode,"tellTurns":self.tell_turns,"canAddTurns":self.can_add_turns(),
            "maxAdditionalTurns":if self.can_add_turns() {MAX_TURNS.min(MAX_TOTAL_TURNS-self.max_turns.max(self.turn))} else {0},
            "maxToolCalls":self.tool_limit(), "canContinue":self.can_continue()})
    }

    /// Build before begin_turn. The previous report is context, never proof of success.
    pub fn prompt_for_next_turn(&self, previous_report: Option<&str>) -> String {
        let previous = previous_report.unwrap_or("");
        let mut end = previous.len().min(6000);
        while !previous.is_char_boundary(end) {
            end -= 1;
        }
        let report = &previous[..end];
        let iteration = if self.mode == RunMode::ResourceLimit {
            "RESOURCE-LIMIT MODE:\nThere is no fixed total turn allowance. Keep advancing the mission until the host stops execution at a configured machine resource limit. Do not voluntarily stop after a small demo or a completed milestone. Do not waste resources on filler or deliberately invalid operations just to force a stop. Build, execute, verify, and improve useful software. A normal end to this turn yields to another iteration."
                .to_string()
        } else {
            let budget = if self.tell_turns {
                format!(
                    "ITERATION {} OF {} ({} turns remaining including this one):",
                    self.turn + 1,
                    self.max_turns,
                    self.max_turns.saturating_sub(self.turn)
                )
            } else {
                "NEXT ITERATION:\nThe operator has not disclosed the turn allowance. Do not infer a remaining-turn count.".into()
            };
            let mode = match self.mode {
                RunMode::Maximize => {
                    "MAXIMIZE MODE: Build the strongest version of the target functionality that fits this machine. Prioritize correctness, useful capabilities, usability, robustness, and efficient resource use. Test weaknesses, optimize implementation and improve the target each iteration while staying inside all hardware limits. Do not fill RAM or disk merely to increase usage. Report evidence and tradeoffs; do not claim a provably optimal result."
                }
                _ => {
                    "STANDARD MODE: Test the previous implementation, then implement and execute the next meaningful milestone. A tiny demonstration or a completed model turn does not end the experiment."
                }
            };
            format!("{budget}\n{mode}")
        };
        format!(
            "EXPERIMENT MISSION:\n{}\n\n{}\nPreserve useful existing work and inspect machine_info for the current source, hardware and remaining resources before editing. Develop the requested system incrementally, including visible output on the guest framebuffer where appropriate. Render text and graphics through your guest assembly; do not ask the host to draw them. Verify observable behavior using execution, memory, screen and serial results, and report what actually works, any failures, and the next milestone. Do not claim success from plans or unexecuted code. Never attempt to alter resource limits.\n\nPREVIOUS TURN REPORT (untrusted context; verify against the machine):\n{}",
            self.mission,
            iteration,
            if report.is_empty() {
                "No previous report."
            } else {
                report
            },
        )
    }

    pub fn begin_turn(&mut self, progress: Progress) -> Result<(), String> {
        if self.status != "waiting"
            || (self.mode != RunMode::ResourceLimit && self.turn >= self.max_turns)
        {
            return Err("This experiment session is not waiting for another turn.".into());
        }
        self.turn = self
            .turn
            .checked_add(1)
            .ok_or("Iteration counter exhausted.")?;
        self.baseline = progress;
        self.status = "running".into();
        self.stop_reason = None;
        self.reason = if self.mode == RunMode::ResourceLimit {
            format!(
                "Working on iteration {} until a machine resource limit is reached.",
                self.turn
            )
        } else {
            format!("Working on iteration {} of {}.", self.turn, self.max_turns)
        };
        Ok(())
    }

    pub fn stop(&mut self, status: &str, reason: &str) {
        self.status = status.into();
        self.reason = reason.into();
        self.stop_reason = match status {
            "waiting" | "running" => None,
            _ => Some(status.into()),
        };
    }
    pub fn stop_resource(&mut self, reason: &str) {
        self.stop("budget", reason);
        self.stop_reason = Some("resource_limit".into());
    }
    pub fn stop_tool_limit(&mut self) {
        self.stop(
            "budget",
            "Agent tool allowance reached. Grant more turns in standard mode to continue.",
        );
        self.stop_reason = Some("tool_limit".into());
    }
    /// Restoring an experiment never starts inference by itself.
    pub fn pause_on_restore(&mut self) {
        if matches!(self.status.as_str(), "running" | "waiting") {
            self.stop("paused", "Saved session restored without starting inference. Continue or grant turns when ready.");
            self.stop_reason = Some("restored".into());
        }
    }

    /// Called exactly once when the current process ends. Budgets span all turns.
    pub fn finish_turn(
        &mut self,
        outcome: &str,
        progress: Progress,
        tool_calls: u64,
        _max_tools: u64,
        max_cycles: u64,
    ) -> bool {
        if self.status != "running" {
            return false;
        }
        if outcome != "completed" {
            let status = if outcome == "stopped" {
                "stopped"
            } else {
                "failed"
            };
            self.stop(
                status,
                "Agent stopped or failed; no automatic retry. Machine state is preserved.",
            );
            return false;
        }
        if progress.cycles >= max_cycles {
            self.stop_resource("Machine cycle allowance reached. Machine state is preserved.");
            return false;
        }
        if self
            .tool_limit()
            .is_some_and(|maximum| tool_calls >= maximum)
        {
            self.stop_tool_limit();
            return false;
        }
        let progressed = progress.memory_revision > self.baseline.memory_revision
            || progress.cycles > self.baseline.cycles
            || progress.disk_touched_bytes > self.baseline.disk_touched_bytes;
        self.stalled_turns = if progressed {
            0
        } else {
            self.stalled_turns.saturating_add(1)
        };
        if self.mode == RunMode::Standard && self.stalled_turns >= 2 {
            self.stop("stalled", "Two turns made no observable machine progress. Grant more turns or resume with a new prompt to continue.");
            return false;
        }
        if self.mode != RunMode::ResourceLimit && self.turn >= self.max_turns {
            self.stop(
                "budget",
                "Turn allowance reached. This does not imply the guest mission is complete.",
            );
            self.stop_reason = Some("turn_limit".into());
            return false;
        }
        self.stop(
            "waiting",
            "Turn ended; preparing the next iteration against the same machine.",
        );
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn session(turns: u64) -> AgentSession {
        AgentSession::new("build a terminal".into(), Some("gpt-6-luna".into()), turns).unwrap()
    }

    #[test]
    fn completed_turn_continues_with_same_mission_and_bounded_report() {
        let mut session = session(6);
        session.begin_turn(Progress::default()).unwrap();
        let progress = Progress {
            memory_revision: 1,
            ..Progress::default()
        };
        assert!(session.finish_turn("completed", progress, 20, 240, 1000));
        assert_eq!(session.state()["status"], "waiting");
        let prompt = session.prompt_for_next_turn(Some(&"z".repeat(20000)));
        assert!(prompt.contains("build a terminal"));
        assert!(prompt.contains("ITERATION 2 OF 6"));
        assert!(prompt.len() < 8000);
        session.begin_turn(progress).unwrap();
        assert_eq!(session.state()["turn"], 2);
    }

    #[test]
    fn failures_stops_and_exhausted_budgets_never_continue() {
        for (outcome, cycles, tools, expected) in [
            ("failed", 1, 1, "failed"),
            ("stopped", 1, 1, "stopped"),
            ("completed", 1000, 1, "budget"),
            ("completed", 1, 240, "budget"),
        ] {
            let mut session = session(6);
            session.begin_turn(Progress::default()).unwrap();
            assert!(!session.finish_turn(
                outcome,
                Progress {
                    cycles,
                    ..Progress::default()
                },
                tools,
                240,
                1000
            ));
            assert_eq!(session.state()["status"], expected);
            assert!(session.begin_turn(Progress::default()).is_err());
        }
    }

    #[test]
    fn two_stalled_turns_stop_but_any_machine_progress_resets_streak() {
        let mut session = session(6);
        let blank = Progress::default();
        session.begin_turn(blank).unwrap();
        assert!(session.finish_turn("completed", blank, 1, 240, 1000));
        session.begin_turn(blank).unwrap();
        let running = Progress { cycles: 1, ..blank };
        assert!(session.finish_turn("completed", running, 2, 240, 1000));
        session.begin_turn(running).unwrap();
        assert!(session.finish_turn("completed", running, 3, 240, 1000));
        session.begin_turn(running).unwrap();
        assert!(!session.finish_turn("completed", running, 4, 240, 1000));
        assert_eq!(session.state()["status"], "stalled");
    }

    #[test]
    fn single_turn_is_explicit_and_terminal_calls_are_idempotent() {
        let mut session = session(1);
        session.begin_turn(Progress::default()).unwrap();
        assert!(!session.finish_turn(
            "completed",
            Progress {
                cycles: 1,
                ..Progress::default()
            },
            1,
            240,
            1000
        ));
        assert_eq!(session.state()["status"], "budget");
        assert!(!session.finish_turn("completed", Progress::default(), 1, 240, 1000));
        assert_eq!(session.state()["status"], "budget");
        assert!(AgentSession::new("go".into(), None, 0).is_err());
        assert!(AgentSession::new("go".into(), None, 25).is_err());
    }
    #[test]
    fn modes_change_continuation_policy_and_prompt_objective() {
        let mut resource = AgentSession::new_with_policy(
            "build a terminal".into(),
            None,
            1,
            RunMode::ResourceLimit,
            true,
        )
        .unwrap();
        assert_eq!(resource.state()["maxTurns"], Value::Null);
        assert_eq!(resource.tool_limit(), None);
        for _ in 0..5 {
            resource.begin_turn(Progress::default()).unwrap();
            assert!(resource.finish_turn("completed", Progress::default(), 10_000, 240, 1000));
        }
        assert!(
            resource
                .prompt_for_next_turn(None)
                .contains("Keep advancing the mission")
        );
        assert!(!resource.can_add_turns());
        resource.stop_resource("Guest disk boundary reached.");
        assert_eq!(resource.state()["stopReason"], "resource_limit");
        assert!(!resource.can_continue());
        assert!(resource.add_turns(1).is_err());

        let mut maximize = AgentSession::new_with_policy(
            "build a terminal".into(),
            None,
            3,
            RunMode::Maximize,
            true,
        )
        .unwrap();
        for _ in 0..2 {
            maximize.begin_turn(Progress::default()).unwrap();
            assert!(maximize.finish_turn("completed", Progress::default(), 1, 240, 1000));
        }
        let prompt = maximize.prompt_for_next_turn(None);
        assert!(prompt.contains("strongest version of the target functionality"));
        assert!(prompt.contains("Do not fill RAM or disk merely"));
        maximize.begin_turn(Progress::default()).unwrap();
        assert!(!maximize.finish_turn("completed", Progress::default(), 1, 240, 1000));
        assert_eq!(maximize.state()["stopReason"], "turn_limit");
        assert!(maximize.add_turns(1).is_err());
    }

    #[test]
    fn hiding_turn_budget_omits_counts_but_keeps_operator_state() {
        let mut session = session(7);
        assert!(
            session
                .prompt_for_next_turn(None)
                .contains("ITERATION 1 OF 7")
        );
        session.set_tell_turns(false);
        let hidden = session.prompt_for_next_turn(None);
        assert!(!hidden.contains(" OF 7"));
        assert!(!hidden.contains("7 turns remaining"));
        assert!(hidden.contains("not disclosed"));
        assert_eq!(session.state()["maxTurns"], 7);
        assert_eq!(session.state()["tellTurns"], false);
        let roundtrip: AgentSession =
            serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
        assert_eq!(roundtrip.state(), session.state());
    }

    #[test]
    fn grant_preserves_progress_and_extends_turn_and_tool_allowance() {
        let mut session = session(1);
        let progress = Progress {
            cycles: 14,
            ..Progress::default()
        };
        session.begin_turn(Progress::default()).unwrap();
        assert!(!session.finish_turn("completed", progress, 12, 240, 1000));
        assert_eq!(session.state()["stopReason"], "turn_limit");
        assert!(session.can_add_turns());
        session.add_turns(2).unwrap();
        assert_eq!(session.state()["turn"], 1);
        assert_eq!(session.state()["maxTurns"], 3);
        assert_eq!(session.tool_limit(), Some(120));
        assert_eq!(session.state()["status"], "waiting");
        session.begin_turn(progress).unwrap();
        session.stop_tool_limit();
        assert_eq!(session.state()["stopReason"], "tool_limit");
        session.add_turns(1).unwrap();
        assert_eq!(session.state()["turn"], 2);
        assert_eq!(session.state()["maxTurns"], 4);
        assert!(session.add_turns(0).is_err());
        assert!(session.add_turns(25).is_err());
        session.max_turns = MAX_TOTAL_TURNS - 1;
        assert_eq!(session.state()["maxAdditionalTurns"], 1);
        assert!(session.add_turns(2).is_err());
        session.add_turns(1).unwrap();
        assert!(!session.can_add_turns());
        assert!(session.add_turns(1).is_err());
    }

    #[test]
    fn grants_during_active_turn_do_not_restart_and_restore_never_autoruns() {
        let mut session = session(2);
        session.begin_turn(Progress::default()).unwrap();
        session.add_turns(2).unwrap();
        assert_eq!(session.state()["status"], "running");
        assert_eq!(session.state()["turn"], 1);
        session.pause_on_restore();
        assert_eq!(session.state()["status"], "paused");
        assert_eq!(session.state()["stopReason"], "restored");
        assert!(session.can_continue());
        assert!(session.can_add_turns());
        session.request_continue().unwrap();
        assert_eq!(session.state()["status"], "waiting");
        session.stop_resource("No cycles remain");
        assert!(session.request_continue().is_err());
        assert!(session.add_turns(1).is_err());
        session.set_mode(RunMode::ResourceLimit);
        assert!(session.request_continue().is_err());
    }
    #[test]
    fn operator_can_restart_a_stopped_session_but_never_refund_resources() {
        let mut session = session(3);
        session.begin_turn(Progress::default()).unwrap();
        session.stop("stopped", "Stopped by operator");
        assert!(session.can_continue());
        assert!(session.can_add_turns());
        session.request_continue().unwrap();
        assert_eq!(session.state()["turn"], 1);
        session.begin_turn(Progress::default()).unwrap();
        session.stop("failed", "Model unavailable");
        assert!(session.can_continue());
        assert!(!session.can_add_turns());
    }
    #[test]
    fn failed_turn_requires_explicit_continuation_and_survives_restore() {
        for mode in [RunMode::Standard, RunMode::Maximize, RunMode::ResourceLimit] {
            let mut session = AgentSession::new_with_policy(
                "build a terminal".into(),
                Some("test-model".into()),
                3,
                mode,
                true,
            )
            .unwrap();
            let progress = Progress {
                cycles: 123,
                memory_revision: 7,
                disk_touched_bytes: 256,
            };
            session.begin_turn(progress).unwrap();
            assert!(!session.finish_turn("failed", progress, 17, 120, 1000));
            assert_eq!(session.state()["status"], "failed");
            assert!(session.begin_turn(progress).is_err());
            let saved = serde_json::to_value(&session).unwrap();
            let mut restored: AgentSession = serde_json::from_value(saved).unwrap();
            restored.pause_on_restore();
            assert_eq!(restored.state()["status"], "failed");
            assert!(restored.can_continue());
            assert!(!restored.can_add_turns());
            restored.request_continue().unwrap();
            assert_eq!(restored.state()["turn"], 1);
            assert_eq!(restored.max_turns, 3);
            assert_eq!(restored.baseline.cycles, 123);
            restored.begin_turn(progress).unwrap();
            assert_eq!(restored.state()["turn"], 2);
            assert_eq!(restored.baseline.memory_revision, 7);
            assert_eq!(restored.baseline.disk_touched_bytes, 256);
            assert_eq!(restored.tool_limit(), session.tool_limit());
        }
    }
    #[test]
    fn failed_final_turn_needs_a_grant_and_resource_stops_remain_final() {
        let mut session = session(1);
        session.begin_turn(Progress::default()).unwrap();
        assert!(!session.finish_turn("failed", Progress::default(), 17, 40, 1000));
        let mut restored: AgentSession =
            serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
        restored.pause_on_restore();
        assert!(!restored.can_continue());
        assert!(restored.can_add_turns());
        assert!(restored.request_continue().is_err());
        restored.add_turns(1).unwrap();
        assert_eq!(restored.state()["turn"], 1);
        assert_eq!(restored.state()["maxTurns"], 2);
        assert_eq!(restored.tool_limit(), Some(80));
        restored.begin_turn(Progress::default()).unwrap();
        restored.stop_resource("RAM boundary reached");
        let mut restored: AgentSession =
            serde_json::from_value(serde_json::to_value(&restored).unwrap()).unwrap();
        restored.pause_on_restore();
        for mode in [RunMode::Standard, RunMode::Maximize, RunMode::ResourceLimit] {
            restored.set_mode(mode);
            assert!(!restored.can_continue());
            assert!(!restored.can_add_turns());
            assert!(restored.request_continue().is_err());
            assert!(restored.add_turns(1).is_err());
        }
    }
    #[test]
    fn unicode_previous_reports_respect_byte_bound() {
        let session = session(6);
        let prompt = session.prompt_for_next_turn(Some(&"🙂".repeat(5000)));
        assert_eq!(prompt.matches('🙂').count(), 1500);
        assert!(prompt.len() < 8000);
    }
    #[test]
    fn dormant_migration_grants_only_the_requested_standard_turn() {
        let mut session =
            AgentSession::dormant("build a terminal".into(), None, RunMode::Standard, true)
                .unwrap();
        assert_eq!(session.state()["status"], "paused");
        assert_eq!(session.state()["stopReason"], "restored");
        assert_eq!(session.state()["turn"], 0);
        assert_eq!(session.state()["maxTurns"], 0);
        assert!(!session.can_continue());
        assert_eq!(session.tool_limit(), Some(0));
        session.add_turns(1).unwrap();
        assert_eq!(session.state()["maxTurns"], 1);
        assert_eq!(session.tool_limit(), Some(40));
        session.begin_turn(Progress::default()).unwrap();
        assert!(!session.finish_turn(
            "completed",
            Progress {
                cycles: 1,
                ..Progress::default()
            },
            1,
            40,
            1000
        ));
        assert_eq!(session.state()["stopReason"], "turn_limit");
        for mode in [RunMode::Maximize, RunMode::ResourceLimit] {
            let dormant = AgentSession::dormant("go".into(), None, mode, true).unwrap();
            assert_eq!(dormant.state()["status"], "paused");
            assert!(dormant.can_continue());
        }
    }

    #[test]
    fn deserialization_rejects_extreme_or_invalid_saved_policy() {
        let original = serde_json::to_value(session(6)).unwrap();
        for (field, value) in [
            ("maxTurns", json!(u64::MAX)),
            ("turn", json!(u64::MAX)),
            ("mission", json!("")),
            ("mission", json!("a".repeat(12001))),
            ("model", json!("bad\nmodel")),
            ("model", json!("a".repeat(121))),
        ] {
            let mut modified = original.clone();
            modified[field] = value;
            assert!(
                serde_json::from_value::<AgentSession>(modified).is_err(),
                "accepted invalid {field}"
            );
        }
        let mut valid = original;
        valid["turn"] = json!(7);
        assert!(
            serde_json::from_value::<AgentSession>(valid).is_ok(),
            "mode changes may retain more consumed turns than the finite allowance"
        );
    }
    #[test]
    fn dormant_standard_can_switch_to_maximize_without_becoming_stuck() {
        let mut session =
            AgentSession::dormant("go".into(), None, RunMode::Standard, true).unwrap();
        session.set_mode(RunMode::Maximize);
        assert_eq!(session.state()["turn"], 0);
        assert_eq!(session.state()["maxTurns"], DEFAULT_TURNS);
        assert_eq!(session.state()["status"], "paused");
        assert!(session.can_continue());
        session.request_continue().unwrap();
        session
            .begin_turn(Progress {
                cycles: 123,
                ..Progress::default()
            })
            .unwrap();
        session.stop("stopped", "operator stop");
        session.set_mode(RunMode::Standard);
        session.set_mode(RunMode::Maximize);
        assert_eq!(session.state()["turn"], 1);
        assert_eq!(session.state()["maxTurns"], DEFAULT_TURNS);
        assert_eq!(session.baseline.cycles, 123);
    }
}
