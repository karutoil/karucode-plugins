//! The stall detector: a pure state machine over server events.
//!
//! Nothing here does I/O. `Detector::observe*` takes one decoded event plus the
//! configured ladder and returns the decisions that event justifies, which makes
//! every threshold testable without a server, a clock or a model.
//!
//! ## What it is looking for
//!
//! Two different failures, because they need different answers:
//!
//! * **A stalled turn** - the model stops working inside one turn and narrates
//!   its intent forever. Upstream (deepseek-recipe#24) this is procedural text -
//!   `OK. Let me write. Go. Now.` - repeated until the output budget is gone, with
//!   no content and no tool call. karucode catches the *exact repetition* case
//!   (one 24-character block eight times) and restarts the stream, but a model
//!   that varies the wording - "Let me write the CSS", "OK, writing it now",
//!   "Let me output the file" - never trips that guard and burns to the cap.
//!   A turn is stalled when it has produced **no tool call and no answer text**
//!   for a long time; the ladder escalates warn -> steer -> cancel.
//!
//! * **A drifting run** - every turn acts, so no single turn is stuck, but the
//!   run spends most of its wall clock narrating before it acts. Measured on a
//!   real session, healthy turns act 2-3 s after they start and stalled ones
//!   spend 6-16 s. No turn-level rule can see this; it is a ratio over the run.
//!
//! ## Why "acting" is the only signal that matters
//!
//! A reasoning model is *supposed* to think. Long reasoning is not a bug, and a
//! turn that thinks for 30 seconds and then calls `write_file` is working
//! correctly. What is never correct is a turn that thinks and then produces
//! nothing at all, because the reasoning channel is **discarded** by karucode -
//! it is streamed to the client and never persisted or replayed. A model that
//! decides something in reasoning and does not act has decided it into the void:
//! the next turn re-derives it from the same context.

use std::collections::HashMap;

use crate::config::Config;

/// Phrases that announce an action instead of taking one.
///
/// Taken from the failure reports, not invented: deepseek-recipe#24 records
/// `OK. Let me write. Go.` / `Let me output.` / `Now.` as the loop's vocabulary,
/// and the same turns on this machine end in `Let me write the file now` with no
/// `write_file` call. Matched case-insensitively against whitespace-collapsed
/// text, so `Let me write.\n\nGo.` and `let me write` both count.
const MARKERS: &[&str] = &[
    "let me write",
    "let me output",
    "let me emit",
    "let me produce",
    "let me print",
    "let me just write",
    "let's write",
    "lets write",
    "time to write",
    "i'll write",
    "i will write",
    "i'll just write",
    "start writing",
    "writing now",
    "now writing",
    "let me go",
    "let me act",
    "let me proceed",
];

/// How much of a delta's tail is kept between scans, so a marker split across two
/// network deltas (`"let me wr"` + `"ite"`) is still counted once.
const CARRY_CHARS: usize = 32;

/// What to do about a stalled turn. Ordered: a turn only ever moves up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    Warn,
    Steer,
    Cancel,
}

impl Rung {
    pub fn as_str(&self) -> &'static str {
        match self {
            Rung::Warn => "warn",
            Rung::Steer => "steer",
            Rung::Cancel => "cancel",
        }
    }
}

/// One thing the detector decided, with the numbers that justified it.
///
/// The caller turns these into I/O; keeping them as data is what lets the panel
/// show what *would* have happened in `dry_run`, and what lets the tests assert
/// on a decision rather than on a side effect.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub session_id: String,
    pub run_id: String,
    /// Present for a turn-level decision, so an escalation can name the turn.
    pub agent_id: String,
    pub kind: DecisionKind,
    /// Why, in one line, for the panel and the notification body.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    /// The human is told, once per turn.
    Warn,
    /// A user turn is folded into the run.
    Steer,
    /// The stream is stopped and a retry queued.
    Cancel,
    /// The run acted every turn but is deliberating far more than it works.
    Drift,
    /// The run ended having produced no tool call and no answer.
    EmptyRun,
}

impl DecisionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionKind::Warn => "warn",
            DecisionKind::Steer => "steer",
            DecisionKind::Cancel => "cancel",
            DecisionKind::Drift => "drift",
            DecisionKind::EmptyRun => "empty_run",
        }
    }
}

/// Counts action-announcing phrases as they stream past.
///
/// Incremental on purpose: a stalled turn can reach hundreds of kilobytes of
/// reasoning, and rescanning all of it on every delta would be quadratic. The
/// carry makes it exact for a marker split across deltas while keeping each scan
/// proportional to the delta.
#[derive(Debug, Default, Clone)]
pub struct Narration {
    carry: String,
    pub hits: usize,
}

impl Narration {
    /// Feed one delta; returns how many markers ended inside it.
    pub fn push(&mut self, delta: &str) -> usize {
        if delta.is_empty() {
            return 0;
        }
        let mut window = String::with_capacity(self.carry.len() + delta.len());
        window.push_str(&self.carry);
        window.push_str(delta);
        let found = count_markers(&window);

        // Keep the tail so the next delta can complete a marker that starts here.
        // Collected through `chars()` so a multi-byte codepoint is never split
        // into invalid UTF-8.
        let mut tail: Vec<char> = window.chars().rev().take(CARRY_CHARS).collect();
        tail.reverse();
        self.carry = tail.into_iter().collect();
        self.hits += found;
        found
    }
}

/// Lowercase, collapse every run of whitespace to one space.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        for lower in ch.to_lowercase() {
            out.push(lower);
        }
    }
    out
}

fn count_markers(window: &str) -> usize {
    let text = normalise(window);
    MARKERS.iter().map(|m| text.matches(m).count()).sum()
}

/// One model turn: from `iteration_started` to the next one, or to the run's end.
#[derive(Debug, Clone)]
struct Turn {
    iteration: u32,
    started_ms: i64,
    reasoning_chars: usize,
    text_chars: usize,
    tool_calls: usize,
    narration: Narration,
    /// Highest rung already taken for this turn, so the ladder never repeats.
    escalated: Option<Rung>,
    /// The turn produced a tool call or answer text: it is working, not stuck.
    acted: bool,
}

impl Turn {
    fn new(iteration: u32, at_ms: i64) -> Self {
        Self {
            iteration,
            started_ms: at_ms,
            reasoning_chars: 0,
            text_chars: 0,
            tool_calls: 0,
            narration: Narration::default(),
            escalated: None,
            acted: false,
        }
    }

    fn elapsed_ms(&self, now_ms: i64) -> i64 {
        (now_ms - self.started_ms).max(0)
    }

    fn elapsed_secs(&self, now_ms: i64) -> u64 {
        (self.elapsed_ms(now_ms) / 1000) as u64
    }
}

/// Totals for a whole run, across every agent in it.
///
/// Deliberately not per agent: drift and an empty run are properties of *the run*,
/// and a subagent's deliberation is spent from the same wall clock and the same
/// user patience as the parent's.
#[derive(Debug, Clone, Default)]
struct Run {
    turns: u32,
    reasoning_chars: usize,
    tool_calls: usize,
    text_chars: usize,
}

/// Identifies the turn in flight. `agent_id` is `"main"` for the top-level loop
/// and the subagent's id otherwise - both are judged, because both are the same
/// model failing the same way.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TurnKey {
    session_id: String,
    run_id: String,
    agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RunKey {
    session_id: String,
    run_id: String,
}

/// The pure detector. Feed it every event from `/api/events`; it keeps only the
/// turn in flight per agent and one totals row per live run.
#[derive(Debug, Default)]
pub struct Detector {
    turns: HashMap<TurnKey, Turn>,
    runs: HashMap<RunKey, Run>,
    /// Last intervention per session, for the cooldown.
    last_intervention_ms: HashMap<String, i64>,
}

impl Detector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Live runs, for the panel.
    pub fn live_runs(&self) -> usize {
        self.runs.len()
    }

    /// Every session with a turn in flight, for the idle tick.
    pub fn sessions(&self) -> Vec<String> {
        let mut out: Vec<String> = self.turns.keys().map(|k| k.session_id.clone()).collect();
        out.sort();
        out.dedup();
        out
    }

    /// The turn in flight for a session, for the panel:
    /// `(iteration, elapsed_secs, reasoning_chars, narration_hits, acted)`.
    pub fn current_turn(&self, session_id: &str, now_ms: i64) -> Option<CurrentTurn> {
        self.turns
            .iter()
            .filter(|(k, _)| k.session_id == session_id)
            .max_by_key(|(_, t)| t.started_ms)
            .map(|(k, t)| CurrentTurn {
                agent_id: k.agent_id.clone(),
                iteration: t.iteration,
                elapsed_secs: t.elapsed_secs(now_ms),
                reasoning_chars: t.reasoning_chars,
                narration_hits: t.narration.hits,
                acted: t.acted,
                escalated: t.escalated.map(|r| r.as_str().to_string()),
            })
    }

    fn turn_mut(
        &mut self,
        session_id: &str,
        run_id: &str,
        agent_id: &str,
        at_ms: i64,
    ) -> &mut Turn {
        let key = TurnKey {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
        };
        self.turns.entry(key).or_insert_with(|| Turn::new(1, at_ms))
    }

    /// Whether the cooldown allows another intervention in this session.
    fn cooled(&self, session_id: &str, now_ms: i64, cfg: &Config) -> bool {
        match self.last_intervention_ms.get(session_id) {
            None => true,
            Some(at) => now_ms - at >= (cfg.cooldown_secs as i64) * 1000,
        }
    }

    // --- the event surface -------------------------------------------------

    pub fn run_started(&mut self, session_id: &str, run_id: &str, _at_ms: i64) {
        self.runs.insert(
            RunKey {
                session_id: session_id.to_string(),
                run_id: run_id.to_string(),
            },
            Run::default(),
        );
    }

    pub fn iteration_started(
        &mut self,
        session_id: &str,
        run_id: &str,
        agent_id: &str,
        iteration: u32,
        at_ms: i64,
    ) -> Vec<Decision> {
        let key = TurnKey {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
        };
        // A new turn replaces the old one. The previous turn is *not* judged here:
        // its decisions were already taken by `tick` while it was in flight, and
        // a turn that ended without a delta is a turn that ended between two
        // deltas - there is nothing new to say about it.
        self.turns.insert(key, Turn::new(iteration, at_ms));
        if let Some(run) = self.runs.get_mut(&RunKey {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
        }) {
            run.turns += 1;
        }
        Vec::new()
    }

    pub fn reasoning(
        &mut self,
        session_id: &str,
        run_id: &str,
        agent_id: &str,
        delta: &str,
        at_ms: i64,
    ) {
        if delta.is_empty() {
            return;
        }
        let chars = delta.chars().count();
        self.turn_mut(session_id, run_id, agent_id, at_ms)
            .narration
            .push(delta);
        let turn = self.turn_mut(session_id, run_id, agent_id, at_ms);
        turn.reasoning_chars += chars;
        if let Some(run) = self.runs.get_mut(&RunKey {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
        }) {
            run.reasoning_chars += chars;
        }
        // Decisions come from `tick`, never from here: a turn's age only advances
        // when the stream does, so the watcher also ticks on an idle timer.
    }

    /// The turn produced answer text: it is working.
    pub fn text(
        &mut self,
        session_id: &str,
        run_id: &str,
        agent_id: &str,
        delta: &str,
        at_ms: i64,
    ) {
        if delta.is_empty() {
            return;
        }
        let chars = delta.chars().count();
        let turn = self.turn_mut(session_id, run_id, agent_id, at_ms);
        turn.text_chars += chars;
        turn.acted = true;
        if let Some(run) = self.runs.get_mut(&RunKey {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
        }) {
            run.text_chars += chars;
        }
    }

    /// The turn called a tool: it is working.
    pub fn tool_call(&mut self, session_id: &str, run_id: &str, agent_id: &str, at_ms: i64) {
        let turn = self.turn_mut(session_id, run_id, agent_id, at_ms);
        turn.tool_calls += 1;
        turn.acted = true;
        if let Some(run) = self.runs.get_mut(&RunKey {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
        }) {
            run.tool_calls += 1;
        }
    }

    /// Evaluate every turn in flight for a session against the ladder.
    ///
    /// Called on every reasoning delta and, from the watcher, on an idle timer -
    /// which matters: a turn that stops sending deltas but keeps the connection
    /// open is the `limit_reached` case, and without the idle tick nothing would
    /// ever look at it again.
    pub fn tick(&mut self, session_id: &str, now_ms: i64, cfg: &Config) -> Vec<Decision> {
        let mut out = Vec::new();
        let keys: Vec<TurnKey> = self
            .turns
            .keys()
            .filter(|k| k.session_id == session_id)
            .cloned()
            .collect();
        for key in keys {
            let Some(turn) = self.turns.get(&key) else {
                continue;
            };
            // A turn that acted is a working turn, however long it thought. This
            // is the single most important condition in the file.
            if turn.acted {
                continue;
            }
            let elapsed = turn.elapsed_secs(now_ms);
            let chars = turn.reasoning_chars;
            let hits = turn.narration.hits;

            let Some(rung) = pick_rung(elapsed, chars, hits, cfg) else {
                continue;
            };
            // Already at or past this rung: the ladder never repeats itself.
            if turn.escalated.is_some_and(|done| rung <= done) {
                continue;
            }
            // The cooldown gates the interventions, not the warning: a warning
            // changes nothing and costs nothing.
            if rung != Rung::Warn && !self.cooled(session_id, now_ms, cfg) {
                continue;
            }

            let reason = format!(
                "turn {iteration} produced no tool call and no answer for {elapsed}s \
                 ({chars} reasoning chars, {hits} action announcement(s))",
                iteration = turn.iteration,
            );
            if let Some(turn) = self.turns.get_mut(&key) {
                turn.escalated = Some(rung);
            }
            if rung != Rung::Warn {
                self.last_intervention_ms
                    .insert(session_id.to_string(), now_ms);
            }
            out.push(Decision {
                session_id: key.session_id.clone(),
                run_id: key.run_id.clone(),
                agent_id: key.agent_id.clone(),
                kind: match rung {
                    Rung::Warn => DecisionKind::Warn,
                    Rung::Steer => DecisionKind::Steer,
                    Rung::Cancel => DecisionKind::Cancel,
                },
                reason,
            });
        }
        out
    }

    /// The run finished. Produces the drift and empty-run decisions, then forgets
    /// everything about the run.
    pub fn run_finished(
        &mut self,
        session_id: &str,
        run_id: &str,
        status: &str,
        cfg: &Config,
    ) -> Vec<Decision> {
        let mut out = Vec::new();
        self.turns
            .retain(|k, _| !(k.session_id == session_id && k.run_id == run_id));

        let key = RunKey {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
        };
        let Some(run) = self.runs.remove(&key) else {
            return out;
        };

        // An empty run is the silent-failure case: no tool calls and no answer
        // text, so the transcript gains an empty assistant row and the status says
        // `completed`. The user cannot tell it apart from a run that answered.
        let empty = run.tool_calls == 0 && run.text_chars == 0 && run.turns_observed() > 0;
        if empty {
            if cfg.report_empty_run {
                out.push(Decision {
                    session_id: session_id.to_string(),
                    run_id: run_id.to_string(),
                    agent_id: "main".to_string(),
                    kind: DecisionKind::EmptyRun,
                    reason: format!(
                        "run ended as `{status}` with no tool call and no answer text, after \
                         {} turn(s) and {} reasoning chars that were discarded",
                        run.turns, run.reasoning_chars
                    ),
                });
            }
        } else if should_report_drift(&run, cfg) {
            out.push(Decision {
                session_id: session_id.to_string(),
                run_id: run_id.to_string(),
                agent_id: "main".to_string(),
                kind: DecisionKind::Drift,
                reason: format!(
                    "run acted {tools} time(s) but spent {chars} reasoning chars doing it \
                     ({ratio} chars per action over {turns} turns)",
                    tools = run.tool_calls,
                    chars = run.reasoning_chars,
                    ratio = run.reasoning_chars / run.tool_calls.max(1),
                    turns = run.turns,
                ),
            });
        }
        let _ = status;
        out
    }
}

/// A snapshot of the turn in flight, for the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentTurn {
    pub agent_id: String,
    pub iteration: u32,
    pub elapsed_secs: u64,
    pub reasoning_chars: usize,
    pub narration_hits: usize,
    pub acted: bool,
    pub escalated: Option<String>,
}

impl Run {
    /// Turns are counted at `iteration_started` *and* on the first delta of a
    /// turn that was never announced separately, so a run with deltas but no
    /// iteration frames still counts as one turn rather than zero.
    fn turns_observed(&self) -> u32 {
        self.turns
    }
}

/// The highest rung the turn currently justifies, or `None`.
///
/// Two signatures, and either is enough:
///
/// 1. **time and volume** - the turn has run for `after_secs` and produced
///    `after_chars` of reasoning. This is the safety net: it catches a loop whose
///    text has no recognizable English in it (the engine-token cycles in
///    deepseek-recipe#24 are 12-20 token periods of punctuation and short words).
/// 2. **time and narration** - the turn has run for `after_secs` and announced an
///    action `narration_min` times. A model that keeps saying it is about to
///    write and never writes is more certain than one that is merely verbose, so
///    this signature does not need the character floor.
fn pick_rung(elapsed_secs: u64, chars: usize, hits: usize, cfg: &Config) -> Option<Rung> {
    for (rung, rule) in [
        (Rung::Cancel, cfg.cancel),
        (Rung::Steer, cfg.steer),
        (Rung::Warn, cfg.warn),
    ] {
        if rule.is_off() || elapsed_secs < rule.after_secs {
            continue;
        }
        if chars >= rule.after_chars || hits >= cfg.narration_min {
            return Some(rung);
        }
    }
    None
}

fn should_report_drift(run: &Run, cfg: &Config) -> bool {
    if cfg.drift.is_off() {
        return false;
    }
    if run.turns < cfg.drift.after_turns || run.reasoning_chars < cfg.drift.after_chars {
        return false;
    }
    if run.tool_calls == 0 {
        return false;
    }
    run.reasoning_chars / run.tool_calls >= cfg.drift.chars_per_tool_call
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Rule;

    /// A ladder that fires quickly, so a test does not have to simulate minutes.
    fn fast() -> Config {
        Config::test_with(|c| {
            c.warn = Rule {
                after_secs: 10,
                after_chars: 100,
            };
            c.steer = Rule {
                after_secs: 20,
                after_chars: 200,
            };
            c.cancel = Rule {
                after_secs: 30,
                after_chars: 300,
            };
            c.cooldown_secs = 0;
        })
    }

    fn kinds(ds: &[Decision]) -> Vec<&str> {
        ds.iter().map(|d| d.kind.as_str()).collect()
    }

    #[test]
    fn a_turn_that_acts_is_never_a_stall_no_matter_how_long_it_thought() {
        let mut d = Detector::new();
        let c = fast();
        d.run_started("s", "r", 0);
        d.reasoning("s", "r", "main", &"thinking hard. ".repeat(500), 0);
        // Stalled: nothing acted and both thresholds are long past.
        assert_eq!(kinds(&d.tick("s", 31_000, &c)), vec!["cancel"]);

        // The same reasoning, but the turn then calls a tool. That retroactively
        // makes all of the thinking legitimate, so nothing fires.
        let mut d = Detector::new();
        d.run_started("s", "r", 0);
        d.reasoning("s", "r", "main", &"thinking hard. ".repeat(500), 0);
        d.tool_call("s", "r", "main", 31_000);
        assert!(d.tick("s", 60_000, &c).is_empty());
    }

    #[test]
    fn answer_text_also_counts_as_acting() {
        let mut d = Detector::new();
        let c = fast();
        d.run_started("s", "r", 0);
        d.reasoning("s", "r", "main", &"x".repeat(500), 0);
        d.text("s", "r", "main", "Here is the answer.", 5_000);
        assert!(d.tick("s", 60_000, &c).is_empty());
    }

    #[test]
    fn the_ladder_escalates_once_per_rung_and_cancel_is_terminal() {
        let mut d = Detector::new();
        let c = fast();
        d.run_started("s", "r", 0);
        d.reasoning("s", "r", "main", &"x".repeat(500), 0);
        assert_eq!(kinds(&d.tick("s", 10_000, &c)), vec!["warn"]);
        // Re-ticking at the same rung must not repeat it.
        assert!(d.tick("s", 11_000, &c).is_empty());
        assert_eq!(kinds(&d.tick("s", 20_000, &c)), vec!["steer"]);
        assert_eq!(kinds(&d.tick("s", 30_000, &c)), vec!["cancel"]);
        assert!(d.tick("s", 90_000, &c).is_empty());
    }

    #[test]
    fn a_turn_below_every_threshold_is_silent() {
        let mut d = Detector::new();
        let c = fast();
        d.run_started("s", "r", 0);
        // 50 chars: under the 100-char warn floor, and only 9s old.
        d.reasoning("s", "r", "main", &"x".repeat(50), 0);
        assert!(d.tick("s", 9_000, &c).is_empty());
    }

    #[test]
    fn the_cooldown_blocks_a_second_intervention_and_expires() {
        let mut d = Detector::new();
        let mut c = fast();
        c.steer = Rule {
            after_secs: 1,
            after_chars: 1,
        };
        c.warn = c.steer;
        c.cooldown_secs = 100;
        d.run_started("s", "r", 0);
        d.reasoning("s", "r", "main", &"x".repeat(50), 0);
        assert_eq!(kinds(&d.tick("s", 2_000, &c)), vec!["steer"]);

        // A new turn immediately afterwards is refused by the cooldown, which is
        // the whole point: a run being stopped gets stopped repeatedly otherwise.
        d.iteration_started("s", "r", "main", 2, 2_100);
        d.reasoning("s", "r", "main", &"x".repeat(50), 2_200);
        assert!(
            d.tick("s", 2_500, &c).is_empty(),
            "the cooldown must survive a new turn"
        );

        // Once it expires the ladder may act again.
        assert_eq!(kinds(&d.tick("s", 200_000, &c)), vec!["steer"]);
    }

    #[test]
    fn the_narration_signature_fires_without_the_character_floor() {
        let mut d = Detector::new();
        let c = Config::test_with(|c| {
            // A floor this turn can never reach.
            c.warn = Rule {
                after_secs: 5,
                after_chars: 100_000,
            };
            c.steer = Rule::OFF;
            c.cancel = Rule::OFF;
            c.narration_min = 3;
        });
        d.run_started("s", "r", 0);
        // The turn's clock starts at its iteration frame, not at its first delta:
        // otherwise a turn that took a while to produce anything would look young.
        d.iteration_started("s", "r", "main", 1, 0);
        // The loop's actual vocabulary, from deepseek-recipe#24.
        for i in 0..5 {
            d.reasoning("s", "r", "main", "OK. Let me write. Go. Now.", 6_000 + i);
        }
        let decisions = d.tick("s", 10_000, &c);
        assert_eq!(kinds(&decisions), vec!["warn"], "{decisions:?}");
        assert!(
            decisions[0].reason.contains("action announcement"),
            "{}",
            decisions[0].reason
        );
    }

    #[test]
    fn narration_is_counted_across_delta_boundaries() {
        let mut n = Narration::default();
        // "let me wr" + "ite" must count once, not zero.
        n.push("... OK, let me wr");
        n.push("ite the file now.");
        assert_eq!(n.hits, 1);
    }

    #[test]
    fn ordinary_reasoning_does_not_register_as_narration() {
        let mut n = Narration::default();
        n.push(
            "The CSP needs frame-ancestors 'self'. I measured the tab width at 68px, so \
             padding 0 6px is the compromise. The flex basis explains the gap.",
        );
        assert_eq!(n.hits, 0);
    }

    #[test]
    fn markers_match_through_newlines_and_capitals() {
        let mut n = Narration::default();
        n.push("Let Me\n\nWrite   the file");
        assert_eq!(n.hits, 1);
    }

    #[test]
    fn a_long_delta_is_scanned_in_linear_time_and_counts_every_hit() {
        let mut n = Narration::default();
        let delta = "let me write the file. ".repeat(400);
        assert_eq!(n.push(&delta), 400);
        assert_eq!(n.hits, 400);
    }

    #[test]
    fn an_empty_run_is_reported_and_a_productive_one_is_not() {
        let c = Config::default();
        let mut d = Detector::new();
        d.run_started("s", "empty", 0);
        d.iteration_started("s", "empty", "main", 1, 0);
        d.reasoning("s", "empty", "main", &"x".repeat(500), 1_000);
        let ds = d.run_finished("s", "empty", "completed", &c);
        assert_eq!(kinds(&ds), vec!["empty_run"], "{ds:?}");
        assert!(ds[0].reason.contains("discarded"), "{}", ds[0].reason);

        let mut d = Detector::new();
        d.run_started("s", "ok", 0);
        d.iteration_started("s", "ok", "main", 1, 0);
        d.text("s", "ok", "main", "Done - all tests pass.", 1_000);
        assert!(d.run_finished("s", "ok", "completed", &c).is_empty());
    }

    #[test]
    fn a_run_that_acted_is_not_reported_as_empty_even_when_it_hit_the_limit() {
        let c = Config::default();
        let mut d = Detector::new();
        d.run_started("s", "r", 0);
        d.iteration_started("s", "r", "main", 1, 0);
        d.tool_call("s", "r", "main", 1_000);
        assert!(d.run_finished("s", "r", "limit_reached", &c).is_empty());
    }

    #[test]
    fn an_empty_run_can_be_silenced() {
        let c = Config::test_with(|c| c.report_empty_run = false);
        let mut d = Detector::new();
        d.run_started("s", "r", 0);
        d.iteration_started("s", "r", "main", 1, 0);
        d.reasoning("s", "r", "main", &"x".repeat(100), 1_000);
        assert!(d.run_finished("s", "r", "completed", &c).is_empty());
    }

    #[test]
    fn drift_needs_turns_volume_and_a_poor_ratio() {
        let c = Config::default();

        // Plenty of turns and volume, but each action is cheap: silent.
        let mut d = Detector::new();
        d.run_started("s", "healthy", 0);
        for i in 1..=20u32 {
            d.iteration_started("s", "healthy", "main", i, (i as i64) * 1_000);
            d.reasoning(
                "s",
                "healthy",
                "main",
                &"y".repeat(1_000),
                (i as i64) * 1_000,
            );
            d.tool_call("s", "healthy", "main", (i as i64) * 1_000 + 20);
        }
        assert!(d.run_finished("s", "healthy", "completed", &c).is_empty());

        // The same shape with a 30k-char debate before each action: drift.
        let mut d = Detector::new();
        d.run_started("s", "drifting", 0);
        for i in 1..=12u32 {
            d.iteration_started("s", "drifting", "main", i, (i as i64) * 1_000);
            d.reasoning(
                "s",
                "drifting",
                "main",
                &"y".repeat(30_000),
                (i as i64) * 1_000,
            );
            d.tool_call("s", "drifting", "main", (i as i64) * 1_000 + 20);
        }
        let ds = d.run_finished("s", "drifting", "completed", &c);
        assert_eq!(kinds(&ds), vec!["drift"], "{ds:?}");
    }

    #[test]
    fn drift_is_reported_once_and_only_at_the_end_of_the_run() {
        let c = Config::default();
        let mut d = Detector::new();
        d.run_started("s", "r", 0);
        for i in 1..=12u32 {
            d.iteration_started("s", "r", "main", i, (i as i64) * 1_000);
            d.reasoning("s", "r", "main", &"y".repeat(30_000), (i as i64) * 1_000);
            d.tool_call("s", "r", "main", (i as i64) * 1_000 + 20);
            // Every turn acted, so the turn-level ladder must stay silent all
            // the way through a drifting run.
            assert!(d.tick("s", (i as i64) * 90_000, &c).is_empty());
        }
        assert_eq!(
            kinds(&d.run_finished("s", "r", "completed", &c)),
            vec!["drift"]
        );
        // A second finish for the same run reports nothing: the row is gone.
        assert!(d.run_finished("s", "r", "completed", &c).is_empty());
    }

    #[test]
    fn a_finished_run_forgets_its_turns_and_runs() {
        let mut d = Detector::new();
        let c = Config::default();
        d.run_started("s", "r", 0);
        d.iteration_started("s", "r", "main", 1, 0);
        d.reasoning("s", "r", "main", &"x".repeat(100), 1_000);
        assert!(d.current_turn("s", 1_500).is_some());
        assert_eq!(d.live_runs(), 1);
        d.run_finished("s", "r", "completed", &c);
        assert!(d.current_turn("s", 2_000).is_none());
        assert_eq!(d.live_runs(), 0);
    }

    #[test]
    fn a_stalled_subagent_is_judged_too() {
        let mut d = Detector::new();
        let c = Config::test_with(|c| {
            c.warn = Rule {
                after_secs: 5,
                after_chars: usize::MAX,
            };
            c.steer = Rule::OFF;
            c.cancel = Rule::OFF;
            c.narration_min = 3;
        });
        d.run_started("s", "r", 0);
        d.iteration_started("s", "r", "a_1", 1, 0);
        for i in 0..4 {
            d.reasoning("s", "r", "a_1", "Let me write. Go. Now.", 1_000 * (i + 1));
        }
        let ds = d.tick("s", 6_000, &c);
        assert_eq!(kinds(&ds), vec!["warn"], "{ds:?}");
        assert_eq!(ds[0].agent_id, "a_1", "the decision must name the subagent");
    }

    #[test]
    fn the_panel_snapshot_reports_the_newest_turn_in_the_session() {
        let mut d = Detector::new();
        d.run_started("s", "r", 0);
        d.iteration_started("s", "r", "main", 3, 1_000);
        d.reasoning("s", "r", "main", "let me write the file", 2_000);
        let t = d.current_turn("s", 12_000).expect("a turn in flight");
        assert_eq!(t.iteration, 3);
        assert_eq!(t.elapsed_secs, 11);
        assert_eq!(t.reasoning_chars, 21);
        assert_eq!(t.narration_hits, 1);
        assert!(!t.acted);
        assert_eq!(t.escalated, None);
    }

    #[test]
    fn reasoning_on_an_unannounced_turn_still_creates_one() {
        // A run that emits deltas before any iteration frame must not be invisible.
        let mut d = Detector::new();
        let mut c = fast();
        c.warn = Rule {
            after_secs: 1,
            after_chars: 1,
        };
        d.reasoning("s", "r", "main", &"x".repeat(100), 0);
        assert_eq!(kinds(&d.tick("s", 5_000, &c)), vec!["warn"]);
    }
}
