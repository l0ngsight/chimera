//! Usage, session and rate limits of agent CLIs: recognising them in a failed turn, working
//! out when they reset, and remembering which runtime/model pairs are limited so personas
//! can switch to their fallbacks (and back) without wasting a turn on a known limit.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Local, NaiveDateTime, NaiveTime, TimeZone, Utc};

use crate::config::RuntimeSpec;
use crate::runtime::TurnOutput;

/// How long a limit is assumed to last when the message does not say.
pub const DEFAULT_COOLDOWN_MINUTES: i64 = 60;
const MAX_COOLDOWN_DAYS: i64 = 8;

/// If the failed turn hit a usage/rate limit, returns the text that says so.
pub fn detect(spec: &RuntimeSpec, output: &TurnOutput) -> Option<String> {
    let err = output.error.as_deref()?;
    // Some CLIs print the limit notice on stdout and only exit non-zero.
    let tail = tail(&output.text, 2000);
    let haystack = format!("{err}\n{tail}").to_lowercase();
    spec.limit_patterns
        .iter()
        .map(|p| p.trim().to_lowercase())
        .any(|p| !p.is_empty() && haystack.contains(&p))
        .then(|| if err.trim().is_empty() { tail.trim().to_string() } else { err.trim().to_string() })
}

fn tail(s: &str, max: usize) -> &str {
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// When the limit described by `msg` resets, e.g. "limit resets 3pm", "Try again in 4 days
/// 20 hours 9 minutes", "usage limit reached|1760000000", "resets 2026-08-08 00:00 UTC".
/// Falls back to [`DEFAULT_COOLDOWN_MINUTES`] from `now`.
pub fn reset_time(msg: &str, now: DateTime<Utc>) -> DateTime<Utc> {
    let lower = msg.to_lowercase();
    // The message comes from agent output, so all arithmetic is checked: absurd values must not panic.
    let parsed = epoch_after_pipe(&lower)
        .or_else(|| relative(&lower).and_then(|d| now.checked_add_signed(d)))
        .or_else(|| absolute(&lower, now));
    let latest = now.checked_add_signed(Duration::days(MAX_COOLDOWN_DAYS)).unwrap_or(now);
    match parsed {
        Some(t) if t > now => t.min(latest),
        // A reset in the past (clock skew, stale message): retry after a short pause.
        Some(_) => now + Duration::minutes(1),
        None => now + Duration::minutes(DEFAULT_COOLDOWN_MINUTES),
    }
}

/// Older Claude Code prints `Claude AI usage limit reached|<unix seconds>`.
fn epoch_after_pipe(s: &str) -> Option<DateTime<Utc>> {
    let rest = &s[s.find('|')? + 1..];
    let digits: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 9 {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    // Milliseconds or seconds.
    if digits.len() >= 13 { DateTime::from_timestamp_millis(n) } else { DateTime::from_timestamp(n, 0) }
}

/// "try again in 4 days 20 hours 9 minutes", "resets in 2h 30m", "retry after 45s".
fn relative(s: &str) -> Option<Duration> {
    let start = ["try again in", "resets in", "reset in", "retry in", "retry after", "available in"]
        .iter()
        .filter_map(|k| s.find(k).map(|i| i + k.len()))
        .min()?;
    let max = Duration::days(MAX_COOLDOWN_DAYS);
    let mut total = Duration::zero();
    let mut found = false;
    let mut tokens = tokenize(&s[start..]).into_iter().peekable();
    while let Some(tok) = tokens.next() {
        let Ok(n) =
            tok.parse::<f64>().map_err(|_| ()).and_then(|n| if n.is_finite() && n >= 0.0 { Ok(n) } else { Err(()) })
        else {
            if matches!(tok.as_str(), "and" | "about" | "approximately" | "~") {
                continue;
            }
            break;
        };
        let Some(unit) = tokens.next() else { break };
        let secs = match unit.trim_end_matches('.') {
            "d" | "day" | "days" => 86_400.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
            "ms" | "millisecond" | "milliseconds" => 0.001,
            _ => break,
        };
        // Anything beyond the cap is clamped later anyway; capping here keeps the sum in range.
        let ms = (n * secs * 1000.0).min(max.num_milliseconds() as f64);
        total = total.checked_add(&Duration::milliseconds(ms as i64)).unwrap_or(max).min(max);
        found = true;
    }
    found.then_some(total)
}

/// Splits "4 days 20h,9m" into ["4", "days", "20", "h", "9", "m"].
fn tokenize(s: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut cur = String::new();
    let mut cur_digit = false;
    for c in s.chars() {
        let digit = c.is_ascii_digit() || (c == '.' && cur_digit);
        if c.is_alphanumeric() || c == '.' || c == '~' {
            if !cur.is_empty() && digit != cur_digit {
                out.push(std::mem::take(&mut cur));
            }
            cur_digit = digit;
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if out.len() > 24 {
            break;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// "resets 3pm", "resets at 3:30 pm", "resets 15:00", "resets 2026-08-08 00:00 utc".
fn absolute(s: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let start = s.find("reset")?;
    let rest = &s[start..];
    let rest = rest.split_once(char::is_whitespace).map(|(_, r)| r)?.trim_start();
    let rest = rest.strip_prefix("at ").unwrap_or(rest).trim_start();
    if let Some(dt) = rest.get(..16).and_then(|d| NaiveDateTime::parse_from_str(d, "%Y-%m-%d %H:%M").ok()) {
        let local = !rest[16..].trim_start().starts_with("utc");
        return if local {
            Local.from_local_datetime(&dt).earliest().map(|t| t.with_timezone(&Utc))
        } else {
            Some(dt.and_utc())
        };
    }
    let time = clock_time(rest)?;
    let today = now.with_timezone(&Local).date_naive();
    let mut at = Local.from_local_datetime(&today.and_time(time)).earliest()?.with_timezone(&Utc);
    if at <= now {
        at += Duration::days(1);
    }
    Some(at)
}

/// "3pm", "3:30 pm", "15:00", "11am)".
fn clock_time(s: &str) -> Option<NaiveTime> {
    let digits_end = s.find(|c: char| !(c.is_ascii_digit() || c == ':')).unwrap_or(s.len());
    let (num, after) = s.split_at(digits_end);
    if num.is_empty() {
        return None;
    }
    let (h, m) = match num.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.get(..2)?.parse::<u32>().ok()?),
        None => (num.parse::<u32>().ok()?, 0),
    };
    let after = after.trim_start();
    let h = if after.starts_with("am") || after.starts_with("a.m") {
        if h == 12 { 0 } else { h }
    } else if after.starts_with("pm") || after.starts_with("p.m") {
        if h == 12 { 12 } else { h + 12 }
    } else if num.contains(':') {
        h
    } else {
        return None; // a bare number is not a time
    };
    NaiveTime::from_hms_opt(h, m, 0)
}

/// Which runtime/model pairs are limited, and until when. Shared by all sessions: a limit
/// belongs to the account the CLI is logged in with, not to one session.
#[derive(Default)]
pub struct Limits {
    until: Mutex<HashMap<(String, String), DateTime<Utc>>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LimitEntry {
    pub runtime: String,
    pub model: String,
    pub until: DateTime<Utc>,
}

impl Limits {
    pub fn record(&self, runtime: &str, model: &str, until: DateTime<Utc>) {
        self.lock().insert((runtime.into(), model.into()), until);
    }

    /// When the pair stops being limited, if it currently is.
    pub fn limited_until(&self, runtime: &str, model: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut map = self.lock();
        map.retain(|_, t| *t > now);
        map.get(&(runtime.to_string(), model.to_string())).copied()
    }

    pub fn list(&self, now: DateTime<Utc>) -> Vec<LimitEntry> {
        let mut map = self.lock();
        map.retain(|_, t| *t > now);
        let mut v: Vec<LimitEntry> = map
            .iter()
            .map(|((runtime, model), until)| LimitEntry {
                runtime: runtime.clone(),
                model: model.clone(),
                until: *until,
            })
            .collect();
        v.sort_by(|a, b| (&a.runtime, &a.model).cmp(&(&b.runtime, &b.model)));
        v
    }

    /// Forgets limits (all of them, or one runtime's) so primaries are tried again.
    pub fn clear(&self, runtime: Option<&str>) {
        self.lock().retain(|(r, _), _| runtime.is_some_and(|x| x != r));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), DateTime<Utc>>> {
        self.until.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_runtimes;

    fn failed(err: &str) -> TurnOutput {
        TurnOutput { error: Some(err.into()), ..Default::default() }
    }

    #[test]
    fn recognises_cli_limit_messages() {
        let rts = builtin_runtimes();
        for (rt, msg) in [
            ("claude", "Claude AI usage limit reached|1760000000"),
            ("claude", "Usage limit reached · limit resets 3pm (Europe/London)"),
            ("claude", "You're out of extra usage · resets 5am"),
            ("claude", "Credit balance is too low"),
            ("codex", "You've hit your usage limit. Try again in 4 days 20 hours 9 minutes."),
            ("codex", "Quota exceeded. Check your plan and billing details."),
            ("codex", "Your workspace is out of credits"),
            ("opencode", "AI_APICallError: 429 Too Many Requests"),
            ("pi", "Rate limit exceeded, please retry after 30s"),
        ] {
            assert!(detect(&rts[rt], &failed(msg)).is_some(), "{rt}: {msg}");
        }
        assert!(detect(&rts["claude"], &failed("`claude` exited with Some(1): ENOENT")).is_none());
        // Successful turns never count, whatever they say.
        let ok = TurnOutput { text: "I fixed the rate limit bug".into(), ..Default::default() };
        assert!(detect(&rts["claude"], &ok).is_none());
        // A non-zero exit whose stdout carries the notice.
        let out = TurnOutput {
            text: "Usage limit reached · limit resets 3pm".into(),
            error: Some("`claude` exited with Some(1)".into()),
            ..Default::default()
        };
        assert!(detect(&rts["claude"], &out).is_some());
    }

    /// Limit messages come from agent output: absurd values are clamped, never a panic.
    #[test]
    fn absurd_reset_times_are_clamped() {
        let now = Utc::now();
        let cap = now + Duration::days(MAX_COOLDOWN_DAYS);
        for msg in [
            "try again in 99999999999999999 days",
            "try again in 99999999999999999 days 99999999999999999 hours",
            "try again in 1e308 seconds",
            "try again in inf days",
            "try again in -5 minutes",
            "usage limit reached|99999999999999999999",
        ] {
            let at = reset_time(msg, now);
            assert!(at > now && at <= cap, "{msg}: {at}");
        }
    }

    #[test]
    fn parses_reset_times() {
        let now = DateTime::from_timestamp(1_760_000_000, 0).unwrap();
        assert_eq!(reset_time("Claude AI usage limit reached|1760003600", now), now + Duration::hours(1));
        assert_eq!(
            reset_time("You've hit your usage limit. Try again in 4 days 20 hours 9 minutes.", now),
            now + Duration::days(4) + Duration::hours(20) + Duration::minutes(9)
        );
        assert_eq!(reset_time("rate limited, try again in 30s", now), now + Duration::seconds(30));
        assert_eq!(reset_time("retry after 2h30m", now), now + Duration::minutes(150));
        assert_eq!(
            reset_time("spend limit reached (daily; resets 2025-10-09 00:00 UTC)", now),
            now + Duration::minutes(1),
            "a reset in the past retries soon"
        );
        assert_eq!(
            reset_time("spend limit reached (daily; resets 2025-10-10 12:00 UTC)", now),
            DateTime::parse_from_rfc3339("2025-10-10T12:00:00Z").unwrap().with_timezone(&Utc)
        );
        // Wall-clock times are local and in the next 24 hours.
        let at = reset_time("Usage limit reached · limit resets 3pm", now);
        assert!(at > now && at <= now + Duration::days(1));
        assert_eq!(at.with_timezone(&Local).time(), NaiveTime::from_hms_opt(15, 0, 0).unwrap());
        let at = reset_time("You're out of extra usage · resets at 5:30 am", now);
        assert_eq!(at.with_timezone(&Local).time(), NaiveTime::from_hms_opt(5, 30, 0).unwrap());
        assert_eq!(reset_time("Quota exceeded.", now), now + Duration::minutes(DEFAULT_COOLDOWN_MINUTES));
        assert_eq!(reset_time("try again in 90 days", now), now + Duration::days(MAX_COOLDOWN_DAYS));
    }

    #[test]
    fn limits_expire() {
        let now = Utc::now();
        let l = Limits::default();
        l.record("claude", "opus", now + Duration::minutes(5));
        l.record("codex", "", now - Duration::minutes(5));
        assert!(l.limited_until("claude", "opus", now).is_some());
        assert!(l.limited_until("claude", "sonnet", now).is_none());
        assert!(l.limited_until("codex", "", now).is_none());
        assert!(l.limited_until("claude", "opus", now + Duration::minutes(6)).is_none());
        l.record("claude", "opus", now + Duration::minutes(5));
        l.clear(Some("codex"));
        assert_eq!(l.list(now).len(), 1);
        l.clear(None);
        assert!(l.list(now).is_empty());
    }
}
