//! Persistent, atomic admission. Every attempted upstream request is charged,
//! including timeouts/429/disconnects. No refunds or resets on reload/restart.
use super::Db;
use crate::free_router::config::Provider;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

const DAY: i64 = 86_400_000;
const MONTH: i64 = 31 * DAY;

#[derive(Default, Serialize, Deserialize)]
struct Remote {
    cooldown_until: i64,
    reason: String,
    requests: Option<RemoteBudget>,
    tokens: Option<RemoteBudget>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct RemoteBudget {
    pub remaining: u64,
    pub reset_ms: i64,
}
#[derive(Default)]
pub struct Feedback {
    pub cooldown_until: i64,
    pub reason: &'static str,
    pub requests: Option<RemoteBudget>,
    pub tokens: Option<RemoteBudget>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Quota {
    pub remaining_requests: u64,
    pub retry_after_s: u64,
    pub reason: String,
}
impl Quota {
    fn wait(&mut self, now: i64, until: i64, reason: &str) {
        let seconds = ((until - now).max(1) as u64).div_ceil(1000);
        if seconds >= self.retry_after_s {
            self.retry_after_s = seconds;
            self.reason = reason.to_string();
        }
    }
}
#[derive(Debug)]
pub struct Reservation {
    pub bucket: String,
    pub id: i64,
}

fn load_remote(conn: &rusqlite::Connection, bucket: &str) -> Result<Remote> {
    let value: Option<String> = conn
        .query_row(
            "SELECT state FROM free_router_limits WHERE bucket=?1",
            [bucket],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match value {
        Some(v) => serde_json::from_str(&v)?,
        None => Remote::default(),
    })
}
fn save_remote(conn: &rusqlite::Connection, bucket: &str, state: &Remote) -> Result<()> {
    conn.execute("INSERT INTO free_router_limits(bucket,state) VALUES(?1,?2) ON CONFLICT(bucket) DO UPDATE SET state=excluded.state", params![bucket, serde_json::to_string(state)?])?;
    Ok(())
}

fn quota(
    conn: &rusqlite::Connection,
    bucket: &str,
    p: &Provider,
    buffer: u32,
    tokens: u64,
    now: i64,
    remote: &Remote,
) -> Result<Quota> {
    let mut q = Quota {
        remaining_requests: u64::MAX,
        retry_after_s: 0,
        reason: "ready".into(),
    };
    for (window, limit, token_limit, label) in [
        (
            60_000,
            Some(p.requests_per_minute),
            false,
            "requests/minute",
        ),
        (3_600_000, p.requests_per_hour, false, "requests/hour"),
        (DAY, p.requests_per_day, false, "requests/day"),
        (MONTH, p.requests_per_month, false, "requests/31 days"),
        (60_000, p.tokens_per_minute, true, "tokens/minute"),
        (DAY, p.tokens_per_day, true, "tokens/day"),
    ] {
        let Some(limit) = limit else {
            continue;
        };
        let (count, total): (u64, u64) = conn.query_row(
            "SELECT COUNT(*),COALESCE(SUM(tokens),0) FROM free_router_requests WHERE bucket=?1 AND ts>?2",
            params![bucket, now - window], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let used = if token_limit { total } else { count };
        let capacity = (limit as u64).saturating_sub(if token_limit { 0 } else { buffer as u64 });
        if !token_limit {
            q.remaining_requests = q.remaining_requests.min(capacity.saturating_sub(used));
        }
        let needed = if token_limit { tokens } else { 1 };
        if used.saturating_add(needed) > capacity {
            // Find the actual earliest rolling-window release, not just the
            // oldest request (several token reservations may need to expire).
            let mut release = now + window;
            let mut remaining = used;
            let mut stmt = conn.prepare("SELECT ts,tokens FROM free_router_requests WHERE bucket=?1 AND ts>?2 ORDER BY ts,id")?;
            let rows = stmt.query_map(params![bucket, now - window], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, u64>(1)?))
            })?;
            for row in rows {
                let (ts, reserved) = row?;
                remaining = remaining.saturating_sub(if token_limit { reserved } else { 1 });
                if remaining.saturating_add(needed) <= capacity {
                    release = ts + window;
                    break;
                }
            }
            q.wait(now, release, label);
        }
    }
    if p.min_interval_ms > 0 {
        let last: Option<i64> = conn.query_row(
            "SELECT MAX(ts) FROM free_router_requests WHERE bucket=?1",
            [bucket],
            |r| r.get(0),
        )?;
        if let Some(until) = last
            .map(|t| t + p.min_interval_ms as i64)
            .filter(|t| *t > now)
        {
            q.wait(now, until, "minimum interval");
        }
    }
    if remote.cooldown_until > now {
        q.wait(now, remote.cooldown_until, &remote.reason);
    }
    for (budget, amount, reserve, label) in [
        (&remote.requests, 1, buffer as u64, "provider request quota"),
        (&remote.tokens, tokens, 0, "provider token quota"),
    ] {
        if let Some(b) = budget.as_ref().filter(|b| b.reset_ms > now) {
            if label == "provider request quota" {
                q.remaining_requests = q
                    .remaining_requests
                    .min(b.remaining.saturating_sub(reserve));
            }
            if b.remaining < amount.saturating_add(reserve) {
                q.wait(now, b.reset_ms, label);
            }
        }
    }
    Ok(q)
}

impl Db {
    pub(super) fn migrate_free_router(conn: &rusqlite::Connection) -> Result<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS free_router_requests(id INTEGER PRIMARY KEY,bucket TEXT NOT NULL,ts INTEGER NOT NULL,tokens INTEGER NOT NULL,pending INTEGER NOT NULL DEFAULT 1);
            CREATE INDEX IF NOT EXISTS free_router_bucket_ts ON free_router_requests(bucket,ts);
            CREATE INDEX IF NOT EXISTS free_router_ts ON free_router_requests(ts);
            CREATE TABLE IF NOT EXISTS free_router_limits(bucket TEXT PRIMARY KEY,state TEXT NOT NULL);")?;
        // Pending hints from interrupted requests expire conservatively after
        // 120 seconds; startup must never reset any quota or concurrent lease.
        Ok(())
    }

    pub fn free_router_quota(
        &self,
        bucket: &str,
        p: &Provider,
        buffer: u32,
        tokens: u64,
        now: i64,
    ) -> Result<Quota> {
        let conn = self.0.lock().unwrap();
        quota(
            &conn,
            bucket,
            p,
            buffer,
            tokens,
            now,
            &load_remote(&conn, bucket)?,
        )
    }

    pub fn reserve_free_request(
        &self,
        bucket: &str,
        p: &Provider,
        buffer: u32,
        tokens: u64,
        now: i64,
    ) -> Result<Result<Reservation, Quota>> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM free_router_requests WHERE ts<=?1",
            [now - MONTH],
        )?;
        let mut remote = load_remote(&tx, bucket)?;
        let q = quota(&tx, bucket, p, buffer, tokens, now, &remote)?;
        if q.retry_after_s > 0 {
            return Ok(Err(q));
        }
        tx.execute(
            "INSERT INTO free_router_requests(bucket,ts,tokens) VALUES(?1,?2,?3)",
            params![bucket, now, tokens],
        )?;
        let id = tx.last_insert_rowid();
        for (budget, amount) in [(&mut remote.requests, 1), (&mut remote.tokens, tokens)] {
            if let Some(b) = budget.as_mut().filter(|b| b.reset_ms > now) {
                b.remaining = b.remaining.saturating_sub(amount);
            }
        }
        save_remote(&tx, bucket, &remote)?;
        tx.commit()?;
        Ok(Ok(Reservation {
            bucket: bucket.into(),
            id,
        }))
    }

    pub fn free_request_feedback(
        &self,
        reservation: &Reservation,
        feedback: Feedback,
        now: i64,
    ) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE free_router_requests SET pending=0 WHERE id=?1",
            [reservation.id],
        )?;
        let mut state = load_remote(&tx, &reservation.bucket)?;
        if feedback.cooldown_until > state.cooldown_until {
            state.cooldown_until = feedback.cooldown_until;
            state.reason = feedback.reason.into();
        }
        // Header responses may arrive out of order. Never increase an unexpired
        // allowance; subtract other requests still pending at the provider.
        let (pending, pending_tokens): (u64,u64) = tx.query_row(
            "SELECT COUNT(*),COALESCE(SUM(tokens),0) FROM free_router_requests WHERE bucket=?1 AND pending=1 AND ts>?2",
            params![reservation.bucket, now-120_000], |r| Ok((r.get(0)?, r.get(1)?)))?;
        for (stored, fresh, pending) in [
            (&mut state.requests, feedback.requests, pending),
            (&mut state.tokens, feedback.tokens, pending_tokens),
        ] {
            if let Some(mut fresh) = fresh.filter(|b| b.reset_ms > now) {
                fresh.remaining = fresh.remaining.saturating_sub(pending);
                if let Some(old) = stored.as_ref().filter(|b| b.reset_ms > now) {
                    fresh.remaining = fresh.remaining.min(old.remaining);
                    fresh.reset_ms = fresh.reset_ms.max(old.reset_ms);
                }
                *stored = Some(fresh);
            }
        }
        save_remote(&tx, &reservation.bucket, &state)?;
        tx.commit()?;
        Ok(())
    }
}
