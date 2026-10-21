//! HTTP client for the Chimera API, including the server-sent event stream of a session.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::model::Team;
use crate::session::{AgentStatus, Event, SessionStatus};

#[derive(Debug, Clone, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub task: String,
    pub team_name: String,
    pub workdir: String,
    pub dry_run: bool,
    pub status: SessionStatus,
    #[serde(default)]
    pub status_detail: Option<String>,
    pub created_at: DateTime<Utc>,
    pub total_turns: u32,
    #[serde(default)]
    pub final_answer: Option<String>,
    pub agents: BTreeMap<String, AgentRow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentRow {
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionDetail {
    pub summary: SessionRow,
    pub team: Team,
}

/// The parts of `/api/settings` the TUI shows.
#[derive(Debug, Clone, Default)]
pub struct SettingsInfo {
    pub guard_on: bool,
    pub guard_detail: String,
    pub auto_routing: bool,
}

#[derive(Clone)]
pub struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    /// `token`: the server's access token, sent with every request (see `access.rs`).
    pub fn new(base: &str, token: Option<String>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(Ok(mut value)) =
            token.filter(|t| !t.is_empty()).map(|t| format!("Bearer {t}").parse::<reqwest::header::HeaderValue>())
        {
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let http = reqwest::Client::builder().default_headers(headers).build().unwrap_or_default();
        Self { base: base.trim_end_matches('/').to_string(), http }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> anyhow::Result<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        decode(resp).await
    }

    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        if resp.status() == reqwest::StatusCode::ACCEPTED || resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        decode(resp).await
    }

    /// Quick reachability check used to decide whether to start an embedded server.
    pub async fn ping(&self) -> bool {
        self.http
            .get(format!("{}/api/teams", self.base))
            .timeout(Duration::from_millis(800))
            .send()
            .await
            // A 401 is a running server that wants a token: connect anyway and report it, rather
            // than starting a second server on the same port.
            .map(|r| r.status().is_success() || r.status() == reqwest::StatusCode::UNAUTHORIZED)
            .unwrap_or(false)
    }

    pub async fn sessions(&self) -> anyhow::Result<Vec<SessionRow>> {
        self.get("/api/sessions").await
    }

    pub async fn teams(&self) -> anyhow::Result<Vec<Team>> {
        self.get("/api/teams").await
    }

    pub async fn session(&self, id: &str) -> anyhow::Result<SessionDetail> {
        self.get(&format!("/api/sessions/{id}")).await
    }

    pub async fn settings(&self) -> anyhow::Result<SettingsInfo> {
        let v: Value = self.get("/api/settings").await?;
        Ok(SettingsInfo {
            guard_on: v["guard"]["available"] == true && v["guard"]["enabled"] == true,
            guard_detail: v["guard"]["detail"].as_str().unwrap_or_default().to_string(),
            auto_routing: v["decisions_active"] == true && v["decisions"]["route_user_messages"] == true,
        })
    }

    pub async fn create_session(&self, team_id: &str, task: &str, dry_run: bool) -> anyhow::Result<SessionRow> {
        let v = self.post("/api/sessions", json!({ "team_id": team_id, "task": task, "dry_run": dry_run })).await?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn post_message(&self, id: &str, to: &str, content: &str) -> anyhow::Result<()> {
        self.post(&format!("/api/sessions/{id}/messages"), json!({ "to": to, "content": content })).await?;
        Ok(())
    }

    pub async fn cancel(&self, id: &str) -> anyhow::Result<()> {
        self.post(&format!("/api/sessions/{id}/cancel"), json!({})).await?;
        Ok(())
    }

    /// Follows a session's event stream (full history first, then live), reconnecting if the
    /// connection drops. A reconnect resumes after the last event received.
    pub async fn follow(self, id: String, tx: mpsc::UnboundedSender<Event>) {
        let mut last = 0u64;
        loop {
            let resp = self.http.get(format!("{}/api/sessions/{id}/stream?after={last}", self.base)).send().await;
            if let Ok(resp) = resp.and_then(|r| r.error_for_status()) {
                let mut parser = SseParser::default();
                let mut body = resp.bytes_stream();
                while let Some(Ok(chunk)) = body.next().await {
                    for data in parser.push(&chunk) {
                        if let Ok(ev) = serde_json::from_str::<Event>(&data) {
                            last = last.max(ev.seq);
                            if tx.send(ev).is_err() {
                                return; // the view was closed
                            }
                        }
                    }
                }
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn decode<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> anyhow::Result<T> {
    let status = resp.status();
    let body: Value = resp.json().await.context("decoding response")?;
    if !status.is_success() {
        bail!("{}", body["error"].as_str().unwrap_or(&format!("HTTP {status}")));
    }
    Ok(serde_json::from_value(body)?)
}

/// Incremental parser for `text/event-stream`: returns the `data` payload of each complete event.
#[derive(Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        // Normalise CRLF so events split on a blank line either way.
        if self.buf.contains('\r') {
            self.buf = self.buf.replace("\r\n", "\n");
        }
        let mut out = vec![];
        while let Some(end) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..end + 2).collect();
            let data: Vec<&str> = block
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_split_chunks_and_keepalives() {
        let mut p = SseParser::default();
        assert!(p.push(b"data: {\"a\":").is_empty());
        assert_eq!(p.push(b"1}\n\n: keep-alive\n\ndata: x\r\n\r\n"), vec!["{\"a\":1}".to_string(), "x".to_string()]);
        assert!(p.push(b"data: partial").is_empty());
    }
}
