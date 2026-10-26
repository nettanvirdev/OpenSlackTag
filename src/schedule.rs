//! Scheduled tasks: "every weekday at 9:00 summarise #support", "in 2h remind
//! me to check the deploy". The teammate creates them with tools; the Slack
//! runner (see `slack.rs`) runs them when due and posts the result.
//!
//! Times are in the server's local time zone (set `TZ` to change it).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, Zoned};
use openslacktag_core::error::{Error, Result as ToolResult};
use openslacktag_core::tool::{PermissionRequest, Tool, ToolContext, ToolOutcome, ToolSource};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::engine::Target;

/// Shortest repeat interval, so a typo cannot run a task every minute.
const MIN_INTERVAL: SignedDuration = SignedDuration::from_mins(5);
const MAX_SCHEDULES: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    /// What to do, as the teammate will read it when the time comes.
    pub prompt: String,
    /// When, as written: `every weekday at 09:00`, `0 9 * * 1-5`, `in 30m`.
    pub when: String,
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub next_run: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Timestamp>,
}

/// A parsed `when`.
#[derive(Debug, Clone, PartialEq)]
enum Spec {
    Once(Timestamp),
    Every(SignedDuration),
    Cron(Cron),
}

impl Spec {
    fn parse(when: &str, now: &Zoned) -> Result<Self> {
        let w = when.trim().to_lowercase();
        if let Some(rest) = w.strip_prefix("in ") {
            return Ok(Self::Once(now.timestamp() + parse_duration(rest)?));
        }
        if let Some(rest) = w.strip_prefix("every ")
            && let Ok(every) = parse_duration(rest)
        {
            if every < MIN_INTERVAL {
                bail!("the shortest interval is 5 minutes");
            }
            return Ok(Self::Every(every));
        }
        if let Some(rest) = w.strip_prefix("at ") {
            let civil: DateTime = rest
                .trim()
                .replace(' ', "T")
                .parse()
                .map_err(|_| anyhow::anyhow!("write the time as `at 2026-11-03 09:00`"))?;
            let at = civil.to_zoned(now.time_zone().clone())?.timestamp();
            if at <= now.timestamp() {
                bail!("{rest} is in the past");
            }
            return Ok(Self::Once(at));
        }
        if let Some(cron) = friendly_to_cron(&w)? {
            return Ok(Self::Cron(Cron::parse(&cron)?));
        }
        Ok(Self::Cron(Cron::parse(&w).map_err(|e| {
            anyhow::anyhow!(
                "{e}. Use `in 30m`, `every 2h`, `at 2026-11-03 09:00`, `every day at 09:00`, \
                 `every weekday at 09:00`, `every monday at 09:00`, or a cron line like `0 9 * * 1-5`"
            )
        })?))
    }

    /// The first run strictly after `after`, or `None` for a one-off already used.
    fn next_after(&self, after: &Zoned) -> Option<Timestamp> {
        match self {
            Self::Once(at) => (*at > after.timestamp()).then_some(*at),
            Self::Every(every) => Some(after.timestamp() + *every),
            Self::Cron(cron) => cron.next_after(after),
        }
    }
}

/// `30m`, `2h`, `1d`, `90 minutes`, `2 hours`, `1 day`.
fn parse_duration(text: &str) -> Result<SignedDuration> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let n: i64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("expected a number in {text:?}"))?;
    let minutes = match unit.trim() {
        "m" | "min" | "mins" | "minute" | "minutes" => n,
        "h" | "hr" | "hrs" | "hour" | "hours" => n * 60,
        "d" | "day" | "days" => n * 60 * 24,
        "w" | "week" | "weeks" => n * 60 * 24 * 7,
        other => bail!("unknown unit {other:?}; use m, h, d or w"),
    };
    if n <= 0 {
        bail!("the duration must be positive");
    }
    Ok(SignedDuration::from_mins(minutes))
}

/// `every day at 09:00`, `every weekday at 9:30`, `every monday at 17:00`.
fn friendly_to_cron(w: &str) -> Result<Option<String>> {
    let Some(rest) = w.strip_prefix("every ") else {
        return Ok(None);
    };
    let Some((days, time)) = rest.split_once(" at ") else {
        return Ok(None);
    };
    let (hour, minute) = time
        .trim()
        .split_once(':')
        .and_then(|(h, m)| Some((h.parse::<u8>().ok()?, m.parse::<u8>().ok()?)))
        .filter(|(h, m)| *h < 24 && *m < 60)
        .ok_or_else(|| anyhow::anyhow!("write the time as 24-hour HH:MM, like 09:00"))?;
    const DAYS: [&str; 7] = [
        "sunday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
    ];
    let dow = match days.trim().trim_end_matches('s') {
        "day" => "*".to_string(),
        "weekday" => "1-5".to_string(),
        "weekend" | "weekend day" => "0,6".to_string(),
        day => match DAYS.iter().position(|d| *d == day) {
            Some(i) => i.to_string(),
            None => return Ok(None),
        },
    };
    Ok(Some(format!("{minute} {hour} * * {dow}")))
}

/// A five-field cron line: minute hour day-of-month month day-of-week, with
/// `*`, lists, ranges and steps. Sunday is 0 or 7.
#[derive(Debug, Clone, PartialEq)]
struct Cron {
    minutes: Vec<bool>,
    hours: Vec<bool>,
    days: Vec<bool>,
    months: Vec<bool>,
    weekdays: Vec<bool>,
    any_day: bool,
    any_weekday: bool,
}

impl Cron {
    fn parse(line: &str) -> Result<Self> {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 5 {
            bail!("a cron line has 5 fields, not {}", fields.len());
        }
        let mut weekdays = field(fields[4], 0, 7)?;
        if weekdays[7] {
            weekdays[0] = true;
        }
        Ok(Self {
            minutes: field(fields[0], 0, 59)?,
            hours: field(fields[1], 0, 23)?,
            days: field(fields[2], 1, 31)?,
            months: field(fields[3], 1, 12)?,
            weekdays,
            any_day: fields[2] == "*",
            any_weekday: fields[4] == "*",
        })
    }

    fn day_matches(&self, z: &Zoned) -> bool {
        let dom = self.days[z.day() as usize];
        let dow = self.weekdays[z.weekday().to_sunday_zero_offset() as usize];
        // Standard cron: when both are restricted, either one matching is enough.
        match (self.any_day, self.any_weekday) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// The first matching minute strictly after `after`, searching a year ahead.
    fn next_after(&self, after: &Zoned) -> Option<Timestamp> {
        let tz = after.time_zone().clone();
        let start = after.timestamp().as_second() / 60 * 60 + 60;
        let mut t = Timestamp::from_second(start).ok()?.to_zoned(tz.clone());
        let limit = after.timestamp() + SignedDuration::from_hours(24 * 366);
        // A hard cap as well as the date limit: a search that cannot finish
        // must not hang the scheduler. A year of minutes is ~527k steps.
        for _ in 0..600_000 {
            if t.timestamp() > limit {
                return None;
            }
            if !self.months[t.month() as usize] || !self.day_matches(&t) {
                // Calendar days, not 24 hours: a day can be 23 or 25 hours long.
                t = t.date().tomorrow().ok()?.to_zoned(tz.clone()).ok()?;
                continue;
            }
            if !self.hours[t.hour() as usize] {
                let next_hour = t.timestamp().as_second() / 3600 * 3600 + 3600;
                t = Timestamp::from_second(next_hour).ok()?.to_zoned(tz.clone());
                continue;
            }
            if self.minutes[t.minute() as usize] {
                return Some(t.timestamp());
            }
            t = t.checked_add(SignedDuration::from_mins(1)).ok()?;
        }
        None
    }
}

/// One cron field as a lookup table indexed by value.
fn field(text: &str, min: usize, max: usize) -> Result<Vec<bool>> {
    let mut on = vec![false; max + 1];
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("bad step in {part:?}"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            bail!("a step of 0 in {part:?}");
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (number(a, min, max)?, number(b, min, max)?)
        } else {
            let n = number(range, min, max)?;
            (n, if step > 1 { max } else { n })
        };
        if lo > hi {
            bail!("range {range:?} runs backwards");
        }
        for v in (lo..=hi).step_by(step) {
            on[v] = true;
        }
    }
    Ok(on)
}

fn number(text: &str, min: usize, max: usize) -> Result<usize> {
    let n: usize = text
        .parse()
        .map_err(|_| anyhow::anyhow!("{text:?} is not a number"))?;
    if n < min || n > max {
        bail!("{n} is outside {min}-{max}");
    }
    Ok(n)
}

/// How long until `when` next comes round, in the server's time zone.
pub fn until_next(when: &str) -> Option<std::time::Duration> {
    let now = Timestamp::now().to_zoned(TimeZone::system());
    let next = Spec::parse(when, &now).ok()?.next_after(&now)?;
    std::time::Duration::try_from(next.duration_since(now.timestamp())).ok()
}

/// Every schedule, saved to `schedules.json` on each change.
#[derive(Debug)]
pub struct Schedules {
    path: PathBuf,
    items: Mutex<Vec<Schedule>>,
    watches_path: PathBuf,
    watches: Mutex<Vec<Watch>>,
    /// Recent trigger times per watch, for the hourly cap.
    fired: Mutex<std::collections::HashMap<String, Vec<Timestamp>>>,
    tz: TimeZone,
}

/// A routine that runs when someone posts in a channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Watch {
    pub id: String,
    pub channel: String,
    /// Only messages containing this text (case-insensitive), if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// A channel trigger runs at most this often per hour, so a busy channel or a
/// loop between bots cannot run up a bill.
const MAX_TRIGGERS_PER_HOUR: usize = 30;

impl Schedules {
    pub fn open(path: PathBuf) -> Self {
        let read = |path: &PathBuf| std::fs::read(path).ok();
        let items = read(&path)
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let watches_path = path.with_file_name("watches.json");
        let watches = read(&watches_path)
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path,
            items: Mutex::new(items),
            watches_path,
            watches: Mutex::new(watches),
            fired: Mutex::default(),
            tz: TimeZone::system(),
        }
    }

    pub fn add_watch(
        &self,
        channel: &str,
        contains: Option<&str>,
        prompt: &str,
        created_by: Option<String>,
    ) -> Result<Watch> {
        let looks_like_id = channel.len() > 8
            && channel.starts_with(['C', 'G'])
            && channel
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if !looks_like_id {
            bail!("give the channel as its id (like C0123456789); slack_list_channels shows them");
        }
        let mut watches = self.watches.lock();
        if watches.len() >= MAX_SCHEDULES {
            bail!("there are already {MAX_SCHEDULES} triggers; delete some first");
        }
        let watch = Watch {
            id: format!("w{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            channel: channel.to_string(),
            contains: contains.map(str::to_string),
            prompt: prompt.trim().to_string(),
            created_by,
        };
        watches.push(watch.clone());
        crate::write_atomic(&self.watches_path, &serde_json::to_vec_pretty(&*watches)?)?;
        Ok(watch)
    }

    pub fn watches(&self) -> Vec<Watch> {
        self.watches.lock().clone()
    }

    /// The triggers a new message in `channel` sets off, within each one's
    /// hourly cap.
    pub fn triggered(&self, channel: &str, text: &str) -> Vec<Watch> {
        let text = text.to_lowercase();
        let now = Timestamp::now();
        let hour_ago = now - SignedDuration::from_hours(1);
        let mut fired = self.fired.lock();
        self.watches
            .lock()
            .iter()
            .filter(|w| w.channel == channel)
            .filter(|w| {
                w.contains
                    .as_deref()
                    .is_none_or(|c| text.contains(&c.to_lowercase()))
            })
            .filter(|w| {
                let times = fired.entry(w.id.clone()).or_default();
                times.retain(|t| *t > hour_ago);
                if times.len() >= MAX_TRIGGERS_PER_HOUR {
                    tracing::warn!("trigger {} hit its hourly cap; skipping", w.id);
                    return false;
                }
                times.push(now);
                true
            })
            .cloned()
            .collect()
    }

    /// Change a routine's prompt, timing (scheduled) or filter (trigger).
    pub fn update(
        &self,
        id: &str,
        prompt: Option<&str>,
        when: Option<&str>,
        contains: Option<&str>,
    ) -> Result<()> {
        {
            let mut items = self.items.lock();
            if let Some(schedule) = items.iter_mut().find(|s| s.id == id) {
                if let Some(when) = when {
                    let now = self.now();
                    let spec = Spec::parse(when, &now)?;
                    schedule.next_run = spec
                        .next_after(&now)
                        .ok_or_else(|| anyhow::anyhow!("{when:?} never happens"))?;
                    schedule.when = when.to_string();
                }
                if let Some(prompt) = prompt {
                    schedule.prompt = prompt.to_string();
                }
                return self.save(&items);
            }
        }
        let mut watches = self.watches.lock();
        let watch = watches
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| anyhow::anyhow!("no routine {id}"))?;
        if when.is_some() {
            bail!(
                "{id} runs on new messages, not on a timer; delete it and create a scheduled routine instead"
            );
        }
        if let Some(prompt) = prompt {
            watch.prompt = prompt.to_string();
        }
        if let Some(contains) = contains {
            watch.contains = (!contains.is_empty()).then(|| contains.to_string());
        }
        crate::write_atomic(&self.watches_path, &serde_json::to_vec_pretty(&*watches)?)
    }

    fn now(&self) -> Zoned {
        Timestamp::now().to_zoned(self.tz.clone())
    }

    pub fn add(
        &self,
        prompt: &str,
        when: &str,
        target: Target,
        created_by: Option<String>,
    ) -> Result<Schedule> {
        let now = self.now();
        let spec = Spec::parse(when, &now)?;
        let next_run = spec
            .next_after(&now)
            .ok_or_else(|| anyhow::anyhow!("{when:?} never happens"))?;
        let mut items = self.items.lock();
        if items.len() >= MAX_SCHEDULES {
            bail!("there are already {MAX_SCHEDULES} schedules; cancel some first");
        }
        let schedule = Schedule {
            id: format!("s{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            prompt: prompt.trim().to_string(),
            when: when.trim().to_string(),
            target,
            created_by,
            next_run,
            last_run: None,
        };
        items.push(schedule.clone());
        self.save(&items)?;
        Ok(schedule)
    }

    pub fn list(&self) -> Vec<Schedule> {
        self.items.lock().clone()
    }

    /// Delete a scheduled routine or a trigger.
    pub fn cancel(&self, id: &str) -> Result<bool> {
        let mut items = self.items.lock();
        let before = items.len();
        items.retain(|s| s.id != id);
        if items.len() != before {
            self.save(&items)?;
            return Ok(true);
        }
        drop(items);
        let mut watches = self.watches.lock();
        let before = watches.len();
        watches.retain(|w| w.id != id);
        if watches.len() != before {
            crate::write_atomic(&self.watches_path, &serde_json::to_vec_pretty(&*watches)?)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Schedules due now. Each is advanced to its next run (or removed, for a
    /// one-off) before it is returned, so a crash mid-run cannot run it twice.
    pub fn take_due(&self) -> Vec<Schedule> {
        let now = self.now();
        let mut items = self.items.lock();
        let mut due = Vec::new();
        let mut keep = Vec::new();
        for mut schedule in items.drain(..) {
            if schedule.next_run > now.timestamp() {
                keep.push(schedule);
                continue;
            }
            due.push(schedule.clone());
            schedule.last_run = Some(now.timestamp());
            let next = Spec::parse(&schedule.when, &now)
                .ok()
                .and_then(|spec| match spec {
                    Spec::Once(_) => None,
                    spec => spec.next_after(&now),
                });
            if let Some(next) = next {
                schedule.next_run = next;
                keep.push(schedule);
            }
        }
        *items = keep;
        if !due.is_empty()
            && let Err(e) = self.save(&items)
        {
            tracing::warn!("could not save schedules: {e:#}");
        }
        due
    }

    /// Move a schedule's next run, so tests can make one due without waiting.
    #[doc(hidden)]
    pub fn set_next_run_for_test(&self, id: &str, at: Timestamp) {
        if let Some(s) = self.items.lock().iter_mut().find(|s| s.id == id) {
            s.next_run = at;
        }
    }

    fn save(&self, items: &[Schedule]) -> Result<()> {
        crate::write_atomic(&self.path, &serde_json::to_vec_pretty(items)?)
    }

    pub fn describe(&self, s: &Schedule) -> String {
        let next = s
            .next_run
            .to_zoned(self.tz.clone())
            .strftime("%a %Y-%m-%d %H:%M");
        let place = match &s.target.thread {
            Some(_) => format!("<#{}> (in a thread)", s.target.channel),
            None => format!("<#{}>", s.target.channel),
        };
        format!(
            "`{}`: {} ({}; next {next}) in {place}",
            s.id,
            crate::one_line(&s.prompt, 120),
            s.when
        )
    }
}

/// The routine tools, bound to the conversation they are used in.
pub fn schedule_tools(
    schedules: &Arc<Schedules>,
    target: &Target,
    requester: Option<&str>,
) -> Vec<Arc<dyn Tool>> {
    [Op::Add, Op::Watch, Op::List, Op::Update, Op::Cancel]
        .into_iter()
        .map(|op| {
            Arc::new(ScheduleTool {
                op,
                schedules: schedules.clone(),
                target: target.clone(),
                requester: requester.map(str::to_string),
            }) as Arc<dyn Tool>
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Add,
    Watch,
    List,
    Update,
    Cancel,
}

#[derive(Debug)]
struct ScheduleTool {
    op: Op,
    schedules: Arc<Schedules>,
    target: Target,
    requester: Option<String>,
}

#[async_trait]
impl Tool for ScheduleTool {
    fn id(&self) -> &str {
        match self.op {
            Op::Add => "schedule_task",
            Op::Watch => "watch_channel",
            Op::List => "schedule_list",
            Op::Update => "schedule_update",
            Op::Cancel => "schedule_cancel",
        }
    }

    fn description(&self) -> &str {
        match self.op {
            Op::Add => {
                "Create a routine: a task that runs later or repeatedly, with the result posted here. `when`: \
                        `in 30m`, `every 2h`, `at 2026-11-03 09:00`, `every day at 09:00`, `every weekday at \
                        09:00`, `every monday at 17:00`, or a cron line like `0 9 * * 1-5`. Times are the server's \
                        local time. Write the prompt so it makes sense on its own later."
            }
            Op::Watch => {
                "Create a routine that runs whenever someone posts in a channel (optionally only messages \
                          containing some text), replying in that message's thread. E.g. triage every new message \
                          in #support. The channel must be one you are in."
            }
            Op::List => "List the routines in this channel: scheduled ones and channel triggers.",
            Op::Update => "Change a routine's prompt or timing, keeping its id and where it posts.",
            Op::Cancel => "Delete a routine by its id.",
        }
    }

    fn parameters(&self) -> Value {
        let (properties, required) = match self.op {
            Op::Add => (
                json!({
                    "prompt": {"type": "string", "description": "What to do each time, self-contained"},
                    "when": {"type": "string"},
                    "in_thread": {"type": "boolean", "description": "Post results in this thread rather than the channel (default false)"}
                }),
                vec!["prompt", "when"],
            ),
            Op::Watch => (
                json!({
                    "channel": {"type": "string", "description": "Channel id, e.g. C0123456789 (`here` for this one)"},
                    "prompt": {"type": "string", "description": "What to do with each matching message"},
                    "contains": {"type": "string", "description": "Optional: only messages containing this text (case-insensitive)"}
                }),
                vec!["channel", "prompt"],
            ),
            Op::List => (json!({}), vec![]),
            Op::Update => (
                json!({
                    "id": {"type": "string"},
                    "prompt": {"type": "string", "description": "Optional: the new prompt"},
                    "when": {"type": "string", "description": "Optional: the new timing (scheduled routines)"},
                    "contains": {"type": "string", "description": "Optional: the new filter (channel triggers; empty for none)"}
                }),
                vec!["id"],
            ),
            Op::Cancel => (json!({"id": {"type": "string"}}), vec!["id"]),
        };
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    }

    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    fn permission(&self, args: &Value) -> PermissionRequest {
        let target = args["when"]
            .as_str()
            .or(args["id"].as_str())
            .or(args["channel"].as_str())
            .unwrap_or("*");
        PermissionRequest::new(self.id(), target)
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult<ToolOutcome> {
        let invalid = |e: anyhow::Error| Error::InvalidInput(format!("{e:#}"));
        let s = |key: &str| args[key].as_str().map(str::trim).filter(|v| !v.is_empty());
        let text = match self.op {
            Op::Add => {
                let prompt =
                    s("prompt").ok_or_else(|| Error::InvalidInput("prompt is empty".into()))?;
                let mut target = self.target.clone();
                if !args["in_thread"].as_bool().unwrap_or(false) {
                    target.thread = None;
                }
                let schedule = self
                    .schedules
                    .add(
                        prompt,
                        s("when").unwrap_or_default(),
                        target,
                        self.requester.clone(),
                    )
                    .map_err(invalid)?;
                format!("Created routine {}", self.schedules.describe(&schedule))
            }
            Op::Watch => {
                let prompt =
                    s("prompt").ok_or_else(|| Error::InvalidInput("prompt is empty".into()))?;
                let channel = match s("channel") {
                    Some("here") | None => self.target.channel.clone(),
                    Some(c) => c
                        .trim_start_matches("<#")
                        .trim_end_matches('>')
                        .split('|')
                        .next()
                        .unwrap_or(c)
                        .to_string(),
                };
                let watch = self
                    .schedules
                    .add_watch(&channel, s("contains"), prompt, self.requester.clone())
                    .map_err(invalid)?;
                format!("Created trigger {}", describe_watch(&watch))
            }
            Op::List => {
                let mut lines: Vec<String> = self
                    .schedules
                    .list()
                    .iter()
                    .filter(|s| s.target.channel == self.target.channel)
                    .map(|s| format!("- {}", self.schedules.describe(s)))
                    .collect();
                lines.extend(
                    self.schedules
                        .watches()
                        .iter()
                        .filter(|w| w.channel == self.target.channel)
                        .map(|w| format!("- {}", describe_watch(w))),
                );
                if lines.is_empty() {
                    "No routines in this channel.".into()
                } else {
                    lines.join("\n")
                }
            }
            Op::Update => {
                let id = s("id").unwrap_or_default();
                self.check_here(id)?;
                self.schedules
                    .update(
                        id,
                        s("prompt"),
                        s("when"),
                        args["contains"].as_str().map(str::trim),
                    )
                    .map_err(invalid)?;
                format!("Updated `{id}`.")
            }
            Op::Cancel => {
                let id = s("id").unwrap_or_default();
                self.check_here(id)?;
                self.schedules
                    .cancel(id)
                    .map_err(|e| Error::Storage(format!("{e:#}")))?;
                format!("Deleted `{id}`.")
            }
        };
        Ok(ToolOutcome::text(text))
    }
}

impl ScheduleTool {
    /// Routines can only be changed from the channel they belong to.
    fn check_here(&self, id: &str) -> ToolResult<()> {
        let here = self
            .schedules
            .list()
            .iter()
            .any(|s| s.id == id && s.target.channel == self.target.channel)
            || self
                .schedules
                .watches()
                .iter()
                .any(|w| w.id == id && w.channel == self.target.channel);
        if here {
            Ok(())
        } else {
            Err(Error::not_found("routine in this channel", id))
        }
    }
}

pub fn describe_watch(w: &Watch) -> String {
    let filter = w
        .contains
        .as_deref()
        .map(|c| format!(" containing \u{201c}{c}\u{201d}"))
        .unwrap_or_default();
    format!(
        "`{}`: {} (on new messages in <#{}>{filter})",
        w.id,
        crate::one_line(&w.prompt, 120),
        w.channel
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn at(text: &str) -> Zoned {
        // A fixed zone keeps the tests independent of the machine's.
        text.parse::<DateTime>()
            .unwrap()
            .to_zoned(TimeZone::get("America/New_York").unwrap())
            .unwrap()
    }

    fn next(when: &str, from: &str) -> String {
        let now = at(from);
        let spec = Spec::parse(when, &now).unwrap();
        spec.next_after(&now)
            .unwrap()
            .to_zoned(now.time_zone().clone())
            .strftime("%a %Y-%m-%d %H:%M")
            .to_string()
    }

    #[test]
    fn friendly_forms() {
        // 2026-10-26 is a Monday.
        assert_eq!(
            next("every weekday at 09:00", "2026-10-26T10:00"),
            "Tue 2026-10-27 09:00"
        );
        assert_eq!(
            next("every weekday at 09:00", "2026-10-30T10:00"),
            "Mon 2026-11-02 09:00"
        );
        assert_eq!(
            next("every day at 9:30", "2026-10-26T08:00"),
            "Mon 2026-10-26 09:30"
        );
        assert_eq!(
            next("every friday at 17:00", "2026-10-26T08:00"),
            "Fri 2026-10-30 17:00"
        );
        assert_eq!(
            next("every mondays at 08:15", "2026-10-26T08:15"),
            "Mon 2026-11-02 08:15"
        );
        assert_eq!(
            next("in 90 minutes", "2026-10-26T08:00"),
            "Mon 2026-10-26 09:30"
        );
        assert_eq!(next("every 2h", "2026-10-26T08:00"), "Mon 2026-10-26 10:00");
        assert_eq!(
            next("at 2026-11-03 09:00", "2026-10-26T08:00"),
            "Tue 2026-11-03 09:00"
        );
    }

    #[test]
    fn cron_lines() {
        assert_eq!(
            next("0 9 * * 1-5", "2026-10-31T12:00"),
            "Mon 2026-11-02 09:00"
        );
        assert_eq!(
            next("*/15 * * * *", "2026-10-26T08:07"),
            "Mon 2026-10-26 08:15"
        );
        assert_eq!(
            next("0 0 1 * *", "2026-10-26T08:00"),
            "Sun 2026-11-01 00:00"
        );
        assert_eq!(
            next("30 18 * * 0", "2026-10-26T08:00"),
            "Sun 2026-11-01 18:30"
        );
        assert_eq!(
            next("30 18 * * 7", "2026-10-26T08:00"),
            "Sun 2026-11-01 18:30"
        );
        // Day of month OR day of week, as in standard cron.
        assert_eq!(
            next("0 12 13 * 5", "2026-10-26T08:00"),
            "Fri 2026-10-30 12:00"
        );
    }

    #[test]
    fn crosses_daylight_saving_time() {
        // US clocks fall back on 2026-11-01.
        assert_eq!(
            next("every day at 09:00", "2026-10-31T10:00"),
            "Sun 2026-11-01 09:00"
        );
    }

    #[test]
    fn triggers_match_filter_cap_and_update() {
        let dir = tempfile::tempdir().unwrap();
        let schedules = Schedules::open(dir.path().join("schedules.json"));
        assert!(
            schedules.add_watch("general", None, "x", None).is_err(),
            "names are refused"
        );
        let any = schedules
            .add_watch("C0123456789", None, "triage", Some("U1".into()))
            .unwrap();
        let urgent = schedules
            .add_watch("C0123456789", Some("URGENT"), "page someone", None)
            .unwrap();
        let ids = |text: &str| -> Vec<String> {
            schedules
                .triggered("C0123456789", text)
                .into_iter()
                .map(|w| w.id)
                .collect()
        };
        assert_eq!(ids("hello"), vec![any.id.clone()]);
        assert_eq!(
            ids("this is urgent!"),
            vec![any.id.clone(), urgent.id.clone()]
        );
        assert!(schedules.triggered("C9999999999", "urgent").is_empty());

        // The hourly cap.
        for _ in 0..MAX_TRIGGERS_PER_HOUR {
            ids("x");
        }
        assert!(!ids("x").contains(&any.id));

        schedules
            .update(&urgent.id, Some("page the on-call"), None, Some(""))
            .unwrap();
        assert!(
            schedules
                .update(&urgent.id, None, Some("every 1h"), None)
                .is_err()
        );
        let reloaded = Schedules::open(dir.path().join("schedules.json"));
        let updated = reloaded
            .watches()
            .into_iter()
            .find(|w| w.id == urgent.id)
            .unwrap();
        assert_eq!(updated.prompt, "page the on-call");
        assert_eq!(updated.contains, None);
        assert!(reloaded.cancel(&any.id).unwrap());
        assert_eq!(reloaded.watches().len(), 1);
    }

    #[test]
    fn scheduled_routines_can_be_updated() {
        let dir = tempfile::tempdir().unwrap();
        let schedules = Schedules::open(dir.path().join("schedules.json"));
        let target = Target {
            channel: "C1".into(),
            thread: None,
        };
        let s = schedules
            .add("a", "every day at 09:00", target, None)
            .unwrap();
        schedules
            .update(&s.id, Some("b"), Some("every 2h"), None)
            .unwrap();
        let updated = &schedules.list()[0];
        assert_eq!(
            (updated.prompt.as_str(), updated.when.as_str()),
            ("b", "every 2h")
        );
        assert!(
            schedules
                .update(&s.id, None, Some("nonsense"), None)
                .is_err()
        );
        assert!(schedules.update("nope", Some("x"), None, None).is_err());
    }

    #[test]
    fn rejects_nonsense() {
        let now = at("2026-10-26T08:00");
        for bad in [
            "every 1m",
            "sometime",
            "at 2020-01-01 09:00",
            "every day at 25:00",
            "61 * * * *",
            "* * *",
            "in 0m",
            "*/0 * * * *",
        ] {
            assert!(Spec::parse(bad, &now).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn due_schedules_advance_and_one_offs_disappear() {
        let dir = tempfile::tempdir().unwrap();
        let schedules = Schedules::open(dir.path().join("schedules.json"));
        let target = Target {
            channel: "C1".into(),
            thread: None,
        };
        let repeating = schedules
            .add("summarise", "every 1h", target.clone(), Some("U1".into()))
            .unwrap();
        let once = schedules.add("remind", "in 5m", target, None).unwrap();
        assert!(schedules.take_due().is_empty());

        // Pretend both are due.
        for s in schedules.items.lock().iter_mut() {
            s.next_run = Timestamp::now() - SignedDuration::from_mins(1);
        }
        let due: Vec<String> = schedules.take_due().into_iter().map(|s| s.id).collect();
        assert_eq!(due.len(), 2);
        let left = schedules.list();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, repeating.id);
        assert!(left[0].next_run > Timestamp::now());
        assert!(!left.iter().any(|s| s.id == once.id));

        // Saved to disk and reloaded.
        let reloaded = Schedules::open(dir.path().join("schedules.json"));
        assert_eq!(reloaded.list(), left);
        assert!(reloaded.cancel(&repeating.id).unwrap());
        assert!(reloaded.list().is_empty());
    }
}
