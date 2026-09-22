//! Informative lifecycle changes and a read-only spending digest. Persistent
//! cursors/intervals avoid heartbeat spam and replay after ordinary restarts.
use super::{alert, backend_ready};
use crate::{db::InstanceRow, state::SharedApp};
use anyhow::{Context, Result};
use serde_json::{json, Value};
#[cfg(test)]
mod tests;

pub async fn run(app: SharedApp) {
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(5));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        timer.tick().await;
        if app.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if let Err(error) = step(&app, chrono::Utc::now().timestamp()) {
            tracing::warn!(%error,"lifecycle notifications deferred; GPU state unchanged");
        }
    }
}

fn short(s: &str) -> String {
    s.chars().take(160).collect()
}
fn state_hint(state: &str) -> &'static str {
    match state {
        "requested"=>"Mietvertrag angelegt; Start steht noch aus.",
        "start_requested"=>"Start angefordert; GPU-Zuweisung noch nicht bestätigt. Keine Bereitschaftszusage, kein ENV-Update. Disk bleibt erhalten.",
        "start_failed"=>"Vast meldet einen Startfehler vor bestätigter Zuweisung. Disk bleibt erhalten; kein blindes Neumieten. Provider-Status prüfen und gezielt erneut starten; Stop/Start übernimmt keine geänderten ENV-Werte.",
        "scheduling"=>"Wartet auf GPU-Kapazität bei Vast. Das ist kein Preempt-/Agent-Fehler. Disk bleibt erhalten; kein Ersatz/Destroy nur wegen der Wartezeit. Budget- und explizite Lifecycle-Regeln gelten weiter.",
        "provisioning"=>"Provider bereitet die Instanz vor; noch nicht einsatzbereit.",
        "agent_connected"=>"Agent verbunden; Service-Bereitschaft wird noch geprüft.",
        "booting"=>"Services/Modell noch nicht bereit oder Healthcheck nicht grün.",
        "healthy"=>"Agent meldet grüne Healthchecks; Router-Freigabe wird separat bestätigt.",
        "unreachable"=>"Agent nicht erreichbar. Das bedeutet nicht automatisch, dass Vast die GPU nicht mehr berechnet.",
        "preempted"=>"Vertrag unterbrochen/preempted; Ersatz richtet sich nach der aktuellen Policy.",
        "draining"=>"Instanz wird aus dem Routing genommen; laufende Arbeit kann noch abschließen.",
        "stopped"=>"Stop bestätigt; gespeicherte Disk/Modelle kosten weiter. Maßgeblich bleibt der Vast-Abgleich.",
        "destroyed"=>"Vertrag lokal als entfernt vermerkt. Abrechnung kann nachlaufen; historische Ausgaben bleiben erhalten. Mietdisk nicht mehr zugesichert.",
        "failed"=>"Instanz im Fehlerzustand. Details im Router-Eventlog; Ursache nicht aus dem Status allein ableitbar.",
        _=>"Tatsächlich gespeicherter Statuswechsel; weitere Details im Router-Eventlog.",
    }
}

fn event_message(app: &SharedApp, event: &crate::db::EventRow) -> Option<(String, String)> {
    let cfg = app.cfg();
    let slot = cfg.slot(event.slot_id?)?;
    let p: Value = serde_json::from_str(&event.payload_json).ok()?;
    if event.kind == "instance_state_changed" {
        let from = p["from"].as_str()?;
        let to = p["to"].as_str()?;
        let rate = p["compute_usd_h"].as_f64()?;
        let storage = p["storage_usd_h"].as_f64()?;
        let message=format!("🔄 Slot {} ({}) · Instanz {}\n{} → {}\nGPU: {} · Modus: {}\nProvider: {} · gewünschter Zustand: {}\nMiete bei laufender GPU: {:.4} USD/h · Speicher: {:.4} USD/h (ohne Traffic)\n{}\nZeitpunkt: {}",slot.id,short(&slot.name),event.instance_id?,short(from),short(to),short(p["gpu"].as_str().unwrap_or("unbekannt")),short(p["mode"].as_str().unwrap_or("unbekannt")),short(p["actual_status"].as_str().unwrap_or("unbekannt")),short(p["intended_status"].as_str().unwrap_or("unbekannt")),rate,storage,state_hint(to),event.ts);
        Some(("instance_state_changed".into(), message))
    } else if event.kind == "slot_lock_changed" {
        let locked=p["locked"].as_i64()?!=0;
        Some(("slot_lock_changed".into(),format!("{} Slot {} ({}) · {}\n{}\nZeitpunkt: {}",if locked {"🔒"} else {"🔓"},slot.id,short(&slot.name),if locked {"LOCKED"} else {"UNLOCKED"},
            if locked {"Automatische Starts/Mieten/Swaps/Stop-/Destroy-Aktionen sind gesperrt, einschließlich Budget-Drain. Laufende Kosten bleiben! Bereits angenommene Aktionen sind ggf. nicht mehr abbrechbar; bewusste Einzelaktionen bleiben möglich."}
            else {"Automatik folgt wieder der aktuellen Konfiguration und den Budgets. Unabhängige Instanz-Pins bleiben erhalten."},event.ts)))
    } else if event.kind == "slot_backend_changed" {
        let previous = p["previous"].as_i64();
        let next = p["next"].as_i64();
        let describe = |id: Option<i64>| {
            id.map(|id| {
                app.db
                    .instance(id)
                    .map(|r| format!("#{id} ({})", short(&r.gpu_name)))
                    .unwrap_or_else(|| format!("#{id}"))
            })
            .unwrap_or_else(|| "kein Backend".into())
        };
        let action = if previous.is_some() && next.is_some() {
            "Backend ersetzt"
        } else {
            "Backend-Zuordnung geändert"
        };
        Some(("slot_backend_changed".into(),format!("🔀 Slot {} ({}) · {action}\n{} → {}\nEinsatzbereitschaft erfordert zusätzlich grüne Healthchecks und Router-Freigabe.\nZeitpunkt: {}",slot.id,short(&slot.name),describe(previous),describe(next),event.ts)))
    } else {
        None
    }
}

fn slot_state(app: &SharedApp, rows: &[InstanceRow]) -> &'static str {
    if rows.iter().any(|r| backend_ready(app, r)) {
        "ready"
    } else if rows.iter().any(|r|r.state=="scheduling") {
        "scheduling (wartet auf GPU-Kapazität)"
    } else if rows.iter().any(|r|r.state=="start_failed") {
        "start_failed (Vast meldet Startfehler; Disk behalten)"
    } else if rows.iter().any(|r|r.state=="start_requested") {
        "start_requested (GPU-Zuweisung unbestätigt)"
    } else if rows.iter().any(|r| {
        matches!(
            r.state.as_str(),
            "requested" | "provisioning" | "booting" | "agent_connected"
        )
    }) {
        "warming"
    } else if rows.iter().any(|r| r.state == "unreachable") {
        "unreachable"
    } else if rows.iter().any(|r| r.state == "healthy") {
        "healthy (Router-Freigabe/Frische ausstehend)"
    } else if rows.is_empty() {
        "cold (keine Mietinstanz)"
    } else {
        "cold (kein freigegebenes Backend)"
    }
}

pub(super) fn step(app: &SharedApp, now: i64) -> Result<()> {
    let cfg = app.cfg();
    let enabled = crate::webhook::targets(&cfg.alerts)?.len() > 0;
    for event in app.db.claim_notification_events()? {
        let age = crate::db::parse_iso(&event.ts)
            .map(|at| now - at.timestamp())
            .unwrap_or(i64::MAX);
        // Do not flood channels with old history after a long outage/rollback.
        if !enabled || !cfg.alerts.state_changes || !(0..=3600).contains(&age) {
            continue;
        }
        if let Some((kind, message)) = event_message(app, &event) {
            alert(app, &kind, &message);
        }
    }
    if app
        .last_reconcile
        .load(std::sync::atomic::Ordering::Relaxed)
        > 0
    {
        let all = app.db.try_instances(false)?;
        for slot in &cfg.slots {
            let rows: Vec<_> = all
                .iter()
                .filter(|r| r.slot_id == slot.id)
                .cloned()
                .collect();
            let state = slot_state(app, &rows);
            if let Some(old) = app
                .db
                .swap_notification_state(&format!("notification_slot_state:{}", slot.id), state)?
            {
                if enabled && cfg.alerts.state_changes {
                    let contracts = rows
                        .iter()
                        .map(|r| format!("#{} {} ({})", r.vast_id, short(&r.gpu_name), r.state))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let message=format!("📍 Slot {} ({})\n{} → {state}\nSoll laufen: {} · Auto-Miete: {}\nVerträge: {}\nSlot-Limit: {:.4} USD/h · globales Limit inkl. Speicher: {:.4} USD/h\nDiese Meldung ändert keine Instanz und kein Budget.",slot.id,short(&slot.name),old,app.db.slot_desired(slot.id),app.db.auto_rent_enabled(),if contracts.is_empty(){"keine"}else{&contracts},slot.bid.ceiling_usd_h,cfg.limits.max_total_rate_usd_h);
                    alert(app, "slot_state_changed", &message);
                }
            }
        }
    }
    let interval = cfg.alerts.spend_summary_interval_s;
    if enabled && interval > 0 {
        // Prepare before claiming: a failed ledger read must not suppress the next attempt.
        let last = app
            .db
            .try_setting("spend_summary_last_attempt")?
            .context("summary interval not initialized")?
            .parse::<i64>()?;
        if now < last || now.saturating_sub(last) >= interval as i64 {
            let message = spend_message(app, now)?;
            if app
                .db
                .claim_interval("spend_summary_last_attempt", now, interval as i64)?
            {
                app.events.emit(
                    &app.db,
                    "spend_summary",
                    None,
                    None,
                    "Periodische Ausgabenübersicht für Webhooks vorbereitet",
                    &json!({"interval_s":interval}),
                );
                alert(app, "spend_summary", &message);
            }
        }
    }
    Ok(())
}

pub(super) fn spend_message(app: &SharedApp, now: i64) -> Result<String> {
    let cfg = app.cfg();
    let policy = crate::reconciler::effective_policy(app);
    let at = chrono::DateTime::from_timestamp(now, 0)
        .context("invalid digest time")?
        .with_timezone(&cfg.router.tz.parse::<chrono_tz::Tz>()?);
    let date = at.format("%Y-%m-%d").to_string();
    let month = at.format("%Y-%m").to_string();
    let (today, month_spend) = app.db.budget_totals(&date)?;
    let exchange = policy.budget.usd_per_eur;
    let rows = app.db.try_instances(false)?;
    let rates = crate::costs::hourly(&rows)?;
    let compute = rates.compute;
    let storage = rates.storage;
    anyhow::ensure!(
        [today, month_spend, compute, storage, exchange]
            .iter()
            .all(|v| v.is_finite())
            && exchange > 0.0,
        "digest cost data unavailable"
    );
    let mut provider=match crate::billing::snapshot(app)? {
        Some(s)=>format!("Vast-Charges: {} {:.3} USD · {} {:.3} USD\nLetzter erfolgreicher Abgleich: {} (Provider kann verzögert sein)",s.day.period,s.day.provider_usd,s.month.period,s.month.provider_usd,s.synced_at),
        None=>"Vast-Charges noch nicht verfügbar — keine Behauptung von Nullkosten.".into(),
    };
    if crate::billing::status(app)["last_attempt"]["state"] == "error" {
        provider.push_str("\nLetzter Charges-Versuch fehlgeschlagen; bestätigte Werte nicht als Echtzeitverbrauch verstehen.");
    }
    let slots = cfg
        .slots
        .iter()
        .map(|slot| {
            let instances = rows
                .iter()
                .filter(|r| r.slot_id == slot.id)
                .map(|r| format!("#{} {}: {}", r.vast_id, short(&r.gpu_name), r.state))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Slot {}: {}",
                slot.id,
                if instances.is_empty() {
                    "cold"
                } else {
                    &instances
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("💰 PGPU-Ausgabenübersicht · {}\nBudgetwirksam heute ({date}): {today:.3} USD / {:.3} EUR\nMonat ({month}): {month_spend:.3} USD / {:.3} EUR\nTageslimit soft/hard: {:.2}/{:.2} EUR · Rest bis hard: {:.3} EUR\nMonatslimit: {:.2} EUR · verbleibend: {:.3} EUR\nAktuelle/reservierte Miete: {compute:.4} USD/h · Speicher: {storage:.4} USD/h\nZusammen: {:.4} USD/h / globales Limit {:.4} USD/h; Traffic zusätzlich\n{provider}\n{slots}\nLokale Schätzungen und bestätigte Mindestkosten; keine Limits oder Instanzen geändert.",at.format("%Y-%m-%d %H:%M %Z"),today/exchange,month_spend/exchange,policy.budget.daily_soft_eur,policy.budget.daily_hard_eur,(policy.budget.daily_hard_eur-today/exchange).max(0.0),policy.budget.monthly_eur,(policy.budget.monthly_eur-month_spend/exchange).max(0.0),compute+storage,policy.limits.max_total_rate_usd_h))
}
