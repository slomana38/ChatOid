//! Persistent automation command parsing and scheduling primitives.
//!
//! This module intentionally does not execute tasks or own persistence. The
//! session layer can serialize these records and decide how to run due work.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Selection;

pub const MIN_LOOP_INTERVAL_SECONDS: u64 = 10;
pub const MAX_LOOP_INTERVAL_SECONDS: u64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutomationCommand {
    LoopStart { interval_seconds: u64, task: String },
    LoopList,
    LoopStop { id: String },
    GoalStart { objective: String },
    GoalStatus,
    GoalStop,
    GoalResume,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    MissingInterval,
    InvalidInterval,
    IntervalOutOfRange,
    MissingTask,
    MissingLoopId,
    MissingObjective,
    MultipleCommands,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::MissingInterval => "Döngü için aralık ve görev gerekli",
            Self::InvalidInterval => {
                "Döngü aralığı geçersiz (10s, 30m veya 1h gibi bir değer kullanın)"
            }
            Self::IntervalOutOfRange => "Döngü aralığı 10 saniye ile 30 gün arasında olmalı",
            Self::MissingTask => "Döngü için görev gerekli",
            Self::MissingLoopId => "Döngüyü durdurmak için kimlik gerekli",
            Self::MissingObjective => "Hedef metni gerekli",
            Self::MultipleCommands => "Bir mesajda yalnızca bir otomasyon komutu kullanın",
        })
    }
}

impl std::error::Error for ParseError {}

/// Return whether text contains a supported automation command token.
///
/// Commands must be whitespace-delimited, so URL paths and words such as
/// `todo/goal` are ignored. Backtick-quoted code is ignored as well.
pub fn contains_command(text: &str) -> bool {
    !find_commands(text).is_empty()
}

/// Parse a supported slash command. Unrecognized commands return `Ok(None)`.
///
/// Supported forms include `/loop 1h task`, `/loop saatlik task`,
/// `/loop list`, `/loop stop <id>`, `/goal <objective>`, `/goal status`,
/// and `/goal stop`.
pub fn parse(text: &str) -> Result<Option<AutomationCommand>, ParseError> {
    let commands = find_commands(text);
    if commands.is_empty() {
        return Ok(None);
    }
    if commands.len() > 1 {
        return Err(ParseError::MultipleCommands);
    }

    let command = commands[0];
    let prefix = text[..command.start].trim();
    let suffix = text[command.end..].trim();

    match command.kind {
        CommandKind::Goal => {
            // Management actions are reserved for clean standalone commands.
            if prefix.is_empty() {
                match suffix.to_ascii_lowercase().as_str() {
                    "status" => return Ok(Some(AutomationCommand::GoalStatus)),
                    "stop" => return Ok(Some(AutomationCommand::GoalStop)),
                    "resume" => return Ok(Some(AutomationCommand::GoalResume)),
                    _ => {}
                }
            }
            let objective = join_text(prefix, suffix);
            if objective.is_empty() {
                return Err(ParseError::MissingObjective);
            }
            Ok(Some(AutomationCommand::GoalStart { objective }))
        }
        CommandKind::Loop => {
            if prefix.is_empty() {
                if suffix.is_empty() || suffix.eq_ignore_ascii_case("list") {
                    return Ok(Some(AutomationCommand::LoopList));
                }
                if suffix.eq_ignore_ascii_case("stop") {
                    return Err(ParseError::MissingLoopId);
                }
                if let Some(id) = strip_keyword(suffix, "stop") {
                    let id = id.trim();
                    if id.is_empty() {
                        return Err(ParseError::MissingLoopId);
                    }
                    return Ok(Some(AutomationCommand::LoopStop { id: id.to_owned() }));
                }
            }

            let (interval, task_suffix) = match suffix.split_once(char::is_whitespace) {
                Some((interval, task_suffix)) => (interval, task_suffix.trim()),
                None if !prefix.is_empty() => (suffix, ""),
                None => {
                    return match parse_interval(suffix) {
                        Ok(_) => Err(ParseError::MissingTask),
                        Err(_) => Err(ParseError::MissingInterval),
                    };
                }
            };
            let task = join_text(prefix, task_suffix);
            if task.is_empty() {
                return Err(ParseError::MissingTask);
            }
            let interval_seconds = parse_interval(interval)?;
            Ok(Some(AutomationCommand::LoopStart {
                interval_seconds,
                task,
            }))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandKind {
    Loop,
    Goal,
}

#[derive(Clone, Copy, Debug)]
struct CommandToken {
    kind: CommandKind,
    start: usize,
    end: usize,
}

fn find_commands(text: &str) -> Vec<CommandToken> {
    let mut found = Vec::new();
    let mut in_code = false;
    let mut previous_was_whitespace = true;
    let mut chars = text.char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        if ch == '`' {
            in_code = !in_code;
            previous_was_whitespace = false;
            continue;
        }
        if !in_code && previous_was_whitespace {
            let rest = &text[index..];
            let candidate = if rest.starts_with("/loop") {
                Some((CommandKind::Loop, "/loop".len()))
            } else if rest.starts_with("/goal") {
                Some((CommandKind::Goal, "/goal".len()))
            } else {
                None
            };
            if let Some((kind, len)) = candidate {
                let end = index + len;
                if text[end..].chars().next().is_none_or(char::is_whitespace) {
                    found.push(CommandToken {
                        kind,
                        start: index,
                        end,
                    });
                    // Skip the recognized token; subsequent iteration starts at its boundary.
                    while chars.peek().is_some_and(|(next, _)| *next < end) {
                        chars.next();
                    }
                    previous_was_whitespace = false;
                    continue;
                }
            }
        }
        previous_was_whitespace = ch.is_whitespace();
    }
    found
}

fn join_text(first: &str, second: &str) -> String {
    match (first.is_empty(), second.is_empty()) {
        (true, true) => String::new(),
        (false, true) => first.to_owned(),
        (true, false) => second.to_owned(),
        (false, false) => format!("{first} {second}"),
    }
}

fn strip_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let (word, rest) = input.split_once(char::is_whitespace)?;
    word.eq_ignore_ascii_case(keyword).then_some(rest)
}

pub(crate) fn parse_interval(value: &str) -> Result<u64, ParseError> {
    if value.eq_ignore_ascii_case("saatlik") {
        return Ok(60 * 60);
    }

    let split_at = value
        .find(|c: char| !c.is_ascii_digit())
        .ok_or(ParseError::InvalidInterval)?;
    let (count, unit) = value.split_at(split_at);
    let count: u64 = count.parse().map_err(|_| ParseError::InvalidInterval)?;
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        "d" | "day" | "days" => 24 * 60 * 60,
        _ => return Err(ParseError::InvalidInterval),
    };
    let seconds = count
        .checked_mul(multiplier)
        .ok_or(ParseError::IntervalOutOfRange)?;
    if !(MIN_LOOP_INTERVAL_SECONDS..=MAX_LOOP_INTERVAL_SECONDS).contains(&seconds) {
        return Err(ParseError::IntervalOutOfRange);
    }
    Ok(seconds)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loop {
    pub id: Uuid,
    pub selection: Selection,
    /// Original source command or message identifier.
    #[serde(default)]
    pub source: i64,
    pub actor: String,
    pub task: String,
    pub interval_seconds: u64,
    /// Unix timestamp for the next scheduled run.
    pub next_run: i64,
    pub active: bool,
    #[serde(default)]
    pub last_job: Option<Uuid>,
}

impl Loop {
    pub fn new(
        selection: Selection,
        actor: impl Into<String>,
        task: impl Into<String>,
        interval_seconds: u64,
        now: i64,
    ) -> Result<Self, ParseError> {
        if !(MIN_LOOP_INTERVAL_SECONDS..=MAX_LOOP_INTERVAL_SECONDS).contains(&interval_seconds) {
            return Err(ParseError::IntervalOutOfRange);
        }
        Ok(Self {
            id: Uuid::new_v4(),
            selection,
            source: 0,
            actor: actor.into(),
            task: task.into(),
            interval_seconds,
            next_run: now.saturating_add(interval_seconds as i64),
            active: true,
            last_job: None,
        })
    }

    /// Advance a due loop to the next future slot, skipping missed intervals.
    /// Returns true when the loop was due and advanced.
    pub fn advance_due(&mut self, now: i64) -> bool {
        if !self.active || self.next_run > now {
            return false;
        }
        let interval = self.interval_seconds.max(1) as i64;
        let elapsed = now.saturating_sub(self.next_run);
        let slots = elapsed / interval + 1;
        self.next_run = self.next_run.saturating_add(slots.saturating_mul(interval));
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Active,
    Complete,
    Stopped,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: Uuid,
    pub selection: Selection,
    /// Original source command or message identifier.
    #[serde(default)]
    pub source: i64,
    pub actor: String,
    pub objective: String,
    pub status: GoalState,
    pub progress: String,
    /// Unix timestamp when this objective was created.
    pub created_at: i64,
    /// Unix timestamp of the last status or progress change.
    pub updated_at: i64,
    /// Cooldown checkpoint used by the goal executor.
    #[serde(default)]
    pub next_run: i64,
    #[serde(default)]
    pub last_job: Option<Uuid>,
    #[serde(default)]
    pub repeated_no_progress: u8,
}

impl Goal {
    pub fn new(
        selection: Selection,
        actor: impl Into<String>,
        objective: impl Into<String>,
        now: i64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            selection,
            source: 0,
            actor: actor.into(),
            objective: objective.into(),
            status: GoalState::Active,
            progress: String::new(),
            created_at: now,
            updated_at: now,
            next_run: now,
            last_job: None,
            repeated_no_progress: 0,
        }
    }

    pub fn set_progress(&mut self, progress: impl Into<String>, now: i64) {
        self.progress = progress.into();
        self.updated_at = now;
    }

    pub fn set_status(&mut self, status: GoalState, now: i64) {
        self.status = status;
        self.updated_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loop_commands_and_units() {
        assert_eq!(
            parse("/loop 1h Check for updates").unwrap(),
            Some(AutomationCommand::LoopStart {
                interval_seconds: 3600,
                task: "Check for updates".into(),
            })
        );
        assert_eq!(
            parse("/loop 30m task").unwrap(),
            Some(AutomationCommand::LoopStart {
                interval_seconds: 1800,
                task: "task".into(),
            })
        );
        assert_eq!(
            parse("/loop 10s task").unwrap(),
            Some(AutomationCommand::LoopStart {
                interval_seconds: 10,
                task: "task".into(),
            })
        );
        assert_eq!(
            parse("/loop saatlik task").unwrap(),
            Some(AutomationCommand::LoopStart {
                interval_seconds: 3600,
                task: "task".into(),
            })
        );
    }

    #[test]
    fn parses_goal_and_management_commands() {
        assert_eq!(parse("/loop").unwrap(), Some(AutomationCommand::LoopList));
        assert_eq!(
            parse("/loop list").unwrap(),
            Some(AutomationCommand::LoopList)
        );
        assert_eq!(
            parse("/loop stop 123").unwrap(),
            Some(AutomationCommand::LoopStop { id: "123".into() })
        );
        assert_eq!(
            parse("/goal Keep improving the report").unwrap(),
            Some(AutomationCommand::GoalStart {
                objective: "Keep improving the report".into(),
            })
        );
        assert_eq!(
            parse("/goal status").unwrap(),
            Some(AutomationCommand::GoalStatus)
        );
        assert_eq!(
            parse("/goal stop").unwrap(),
            Some(AutomationCommand::GoalStop)
        );
        assert_eq!(
            parse("/goal resume").unwrap(),
            Some(AutomationCommand::GoalResume)
        );
    }

    #[test]
    fn rejects_invalid_loop_inputs_and_ignores_other_commands() {
        assert_eq!(parse("/other test").unwrap(), None);
        assert_eq!(parse("/loop 9s task"), Err(ParseError::IntervalOutOfRange));
        assert_eq!(parse("/loop 31d task"), Err(ParseError::IntervalOutOfRange));
        assert_eq!(parse("/loop 1x task"), Err(ParseError::InvalidInterval));
        assert_eq!(parse("/loop 1h"), Err(ParseError::MissingTask));
        assert_eq!(parse("/loop 1h   "), Err(ParseError::MissingTask));
        assert_eq!(parse("/loop stop"), Err(ParseError::MissingLoopId));
        assert_eq!(parse("/goal   "), Err(ParseError::MissingObjective));
    }

    #[test]
    fn parses_commands_inside_prose_and_ignores_non_command_slashes() {
        assert_eq!(
            parse("bir daha dene /goal").unwrap(),
            Some(AutomationCommand::GoalStart {
                objective: "bir daha dene".into(),
            })
        );
        assert_eq!(
            parse("lütfen /goal dosyayı oluştur").unwrap(),
            Some(AutomationCommand::GoalStart {
                objective: "lütfen dosyayı oluştur".into(),
            })
        );
        assert_eq!(
            parse("/goal status").unwrap(),
            Some(AutomationCommand::GoalStatus)
        );
        assert_eq!(
            parse("lütfen /goal status").unwrap(),
            Some(AutomationCommand::GoalStart {
                objective: "lütfen status".into(),
            })
        );
        assert_eq!(
            parse("task /loop 1h").unwrap(),
            Some(AutomationCommand::LoopStart {
                interval_seconds: 3600,
                task: "task".into(),
            })
        );
        assert_eq!(
            parse("lütfen /loop 1h task").unwrap(),
            Some(AutomationCommand::LoopStart {
                interval_seconds: 3600,
                task: "lütfen task".into(),
            })
        );
        assert_eq!(
            parse("https://example.test/path/goal todo/goal").unwrap(),
            None
        );
        assert_eq!(
            parse("`/goal hidden` https://example.test/loop").unwrap(),
            None
        );
        assert!(!contains_command("foo/goal https://example.test/path/loop"));
        assert!(contains_command("Türkçe /goal hedef"));
    }

    #[test]
    fn rejects_multiple_inline_automation_commands() {
        assert_eq!(
            parse("/goal one and /loop 1h two"),
            Err(ParseError::MultipleCommands)
        );
        assert!(contains_command("/goal one and /loop 1h two"));
    }

    #[test]
    fn overdue_loop_skips_missed_slots_without_catch_up_burst() {
        let selection = Selection {
            host: "host".into(),
            room: Uuid::new_v4(),
        };
        let mut loop_job = Loop::new(selection, "codex", "task", 10, 100).unwrap();
        assert_eq!(loop_job.next_run, 110);
        assert!(loop_job.advance_due(155));
        assert_eq!(loop_job.next_run, 160);
        assert!(!loop_job.advance_due(159));
        loop_job.active = false;
        assert!(!loop_job.advance_due(200));
    }
}
