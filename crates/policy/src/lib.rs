//! Reine Policy-Funktionen: `decide(snapshot, cfg) -> Vec<Action>`.
//! Kein I/O, keine Zeitmessung — alles kommt über den Snapshot. 100 % testbar.

pub mod schedule;

use chrono::{DateTime, Utc};
use praxis_common::{Action, InstanceState, Mode, Role};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- Konfiguration

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    pub daily_soft_eur: f64,
    pub daily_hard_eur: f64,
    #[serde(default = "d_monthly")]
    pub monthly_eur: f64,
    pub usd_per_eur: f64,
}

fn d_monthly() -> f64 {
    50.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "d_max_instances")]
    pub max_instances: i64,
    #[serde(default = "d_max_per_slot")]
    pub max_per_slot: i64,
    #[serde(default = "d_max_rate")]
    pub max_total_rate_usd_h: f64,
}

fn d_max_instances() -> i64 {
    3
}
fn d_max_per_slot() -> i64 {
    2
}
fn d_max_rate() -> f64 {
    0.60
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BidConfig {
    #[serde(default = "d_margin")]
    pub margin: f64,
    pub ceiling_usd_h: f64,
    #[serde(default = "d_defend")]
    pub defend_when_busy: bool,
}

fn d_margin() -> f64 {
    0.15
}
fn d_defend() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleConfig {
    pub stop_after_s: i64,
    pub destroy_after_stopped_s: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwapConfig {
    #[serde(default = "d_true")]
    pub on_preempt: bool,
    #[serde(default = "d_true")]
    pub on_bid_pressure: bool,
    #[serde(default)]
    pub optimize_cost: bool,
    #[serde(default = "d_savings")]
    pub min_savings_pct: f64,
    #[serde(default = "d_warmup")]
    pub max_warmup_s: i64,
    /// Max 1 Swap/Slot/Stunde (Hysterese).
    #[serde(default = "d_swap_h")]
    pub min_swap_interval_s: i64,
    /// Traffic-Fenster: nur bei Nutzung in den letzten X Minuten Replacement.
    #[serde(default = "d_keep_warm")]
    pub keep_warm_window_s: i64,
}

fn d_true() -> bool {
    true
}
fn d_savings() -> f64 {
    25.0
}
fn d_warmup() -> i64 {
    2700
}
fn d_swap_h() -> i64 {
    3600
}
fn d_keep_warm() -> i64 {
    1800
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotPolicyCfg {
    pub bid: BidConfig,
    pub idle: IdleConfig,
    #[serde(default)]
    pub swap: SwapConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    pub budget: BudgetConfig,
    pub limits: LimitsConfig,
    pub slots: std::collections::HashMap<i64, SlotPolicyCfg>,
}

// ---------------------------------------------------------------- Snapshot

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OfferSnapshot {
    pub id: i64,
    /// Vast-Maschine (Blacklist-Key); 0 = unbekannt (alte Caches).
    #[serde(default)]
    pub machine_id: i64,
    pub gpu_name: String,
    pub min_bid: f64,
    pub dph_total: f64,
    pub storage_cost: f64, // $/GB/Monat
    pub inet_down_cost: f64,
    // Anzeige-Felder (Dashboard), Policy ignoriert sie.
    #[serde(default)]
    pub cpu_ram_gb: f64,
    #[serde(default)]
    pub gpu_ram_gb: f64,
    #[serde(default)]
    pub disk_gb: f64,
    #[serde(default)]
    pub inet_down: f64,
    #[serde(default)]
    pub reliability2: f64,
    #[serde(default)]
    pub disk_bw: f64,
}

impl OfferSnapshot {
    /// Score nach Bauplan 4.6: Rate + Storage + anteiliger Traffic-Kosten,
    /// **gewichtet mit reliability2** — erwartete nutzbare Stunden pro gemieteter
    /// Stunde = reliability2, daher effektive Kosten = Basis / reliability.
    /// Spike-Lektion: die billigsten Hosts (0.95) starben reihenweise vast-seitig;
    /// ohne Gewichtung mietet Wake-Replace immer auf den wackligsten Host.
    /// Fehlende reliability2 (0.0, alte Caches) → neutral 0.95 annehmen.
    pub fn score(&self, disk_gb: i64, traffic_gb: f64, expected_hours: f64) -> f64 {
        let storage = self.storage_cost * disk_gb as f64 / 30.0 / 24.0;
        let traffic = self.inet_down_cost * traffic_gb;
        let rate = if self.min_bid > 0.0 { self.min_bid } else { self.dph_total };
        let reliability = if self.reliability2 > 0.0 {
            self.reliability2.clamp(0.5, 1.0)
        } else {
            0.95
        };
        (rate + storage + traffic / expected_hours.max(0.5)) / reliability
    }
}

#[derive(Debug, Clone)]
pub struct InstanceSnapshot {
    pub vast_id: i64,
    pub offer_id: i64,
    pub machine_id: i64,
    pub gpu_name: String,
    pub role: Role,
    pub slot_id: i64,
    pub mode: Mode,
    pub lifecycle: praxis_common::Lifecycle,
    pub state: InstanceState,
    pub actual_status: String,
    pub intended_status: String,
    pub healthy: bool,
    pub busy: bool,
    pub busy_reason: String,
    pub min_bid: f64,
    pub bid_usd_h: f64,
    pub dph_total: f64,
    pub storage_usd_h: f64,
    pub created_at: DateTime<Utc>,
    pub idle_since: Option<DateTime<Utc>>,
    pub stopped_since: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    pub pinned: bool,
}

impl InstanceSnapshot {
    fn rate_usd_h(&self) -> f64 {
        match self.mode {
            Mode::Interruptible => self.bid_usd_h,
            _ => self.dph_total,
        }
    }
    pub fn is_running(&self) -> bool {
        self.actual_status == "running" && self.state.is_active()
    }
}

#[derive(Debug, Clone)]
pub struct SlotSnapshot {
    pub id: i64,
    pub role: Role,
    pub name: String,
    pub pinned: bool,
    /// Instanz, die aktuell den Slot-Backer darstellt.
    pub active_instance: Option<i64>,
    pub instances: Vec<InstanceSnapshot>,
    /// Offene Requests/WS am Router für diesen Slot.
    pub in_flight: u32,
    /// Letzter Request (für idle/busy_grace) — final von Reconciler vorgekaut.
    pub last_traffic: Option<DateTime<Utc>>,
    pub last_swap: Option<DateTime<Utc>>,
    /// Bester Kandidat aus der Angebotssuche (vom Reconciler).
    pub candidate_offer: Option<OfferSnapshot>,
    /// Slot soll laufen (Wake, Schedule).
    pub desired_running: bool,
    /// Lokale Zeit für Zeitpläne (Reconciler rechnet TZ um).
    pub local_weekday: u8, // 1 = Mo … 7 = So
    pub local_minutes_of_day: u32,
}

impl SlotSnapshot {
    pub fn active(&self) -> Option<&InstanceSnapshot> {
        self.active_instance.and_then(|id| self.instances.iter().find(|i| i.vast_id == id))
    }
    pub fn healthy_replacement(&self) -> Option<&InstanceSnapshot> {
        self.instances
            .iter()
            .find(|i| Some(i.vast_id) != self.active_instance && i.state == InstanceState::Healthy && i.is_running())
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub now: DateTime<Utc>,
    pub seconds_to_day_end: i64,
    pub spent_today_usd: f64,
    pub spent_month_usd: f64,
    pub slots: Vec<SlotSnapshot>,
    pub instance_count: usize,
    /// Laufender Stundenpreis (running) inkl. Storage gestoppter.
    pub running_rate_usd_h: f64,
    pub storage_rate_usd_h: f64,
}

impl Snapshot {
    /// Projektion bis Tagesende (nur für Alerts/Berichte: „wenn die Box
    /// durchläuft, kostet sie X bis Mitternacht").
    pub fn projected(&self, extra_rate_usd_h: f64, removed_rate_usd_h: f64, extra_one_off_usd: f64) -> f64 {
        let hours = (self.seconds_to_day_end.max(0) as f64) / 3600.0;
        (self.running_rate_usd_h + extra_rate_usd_h - removed_rate_usd_h).max(0.0) * hours
            + self.storage_rate_usd_h * hours
            + self.spent_today_usd
            + extra_one_off_usd
    }

    /// Aktionsfenster laut Bauplan: „projected(startup + 30 min) ≤ soft".
    /// Bei Interruptibles begrenzt der Idle-Stop die reale Laufzeit;
    /// die Mitternachts-Projektion würde jede frisch gemietete Box
    /// fälschlich über den Cap werfen.
    pub fn projected_30m(&self, extra_rate_usd_h: f64, removed_rate_usd_h: f64, extra_one_off_usd: f64) -> f64 {
        let hours = (self.seconds_to_day_end.max(0).min(1800)) as f64 / 3600.0;
        (self.running_rate_usd_h + extra_rate_usd_h - removed_rate_usd_h).max(0.0) * hours
            + self.storage_rate_usd_h * hours
            + self.spent_today_usd
            + extra_one_off_usd
    }
}

// ---------------------------------------------------------------- Decide

/// Policy-Kern. Liefert Aktionen, sortiert nach Priorität.
pub fn decide(snap: &Snapshot, cfg: &PolicyConfig) -> Vec<Action> {
    let mut actions = Vec::new();
    let soft_usd = cfg.budget.daily_soft_eur * cfg.budget.usd_per_eur;
    let hard_usd = cfg.budget.daily_hard_eur * cfg.budget.usd_per_eur;
    let monthly_usd = cfg.budget.monthly_eur * cfg.budget.usd_per_eur;
    let hours_to_end = (snap.seconds_to_day_end.max(0) as f64) / 3600.0;

    // --- Budget-Hard-Cap: real akkumulierte Kosten ueberstiegen -> alles drainen.
    if snap.spent_today_usd >= hard_usd {
        for slot in &snap.slots {
            for inst in &slot.instances {
                if inst.is_running() {
                    actions.push(Action::Stop {
                        instance_id: inst.vast_id,
                        reason: format!(
                            "budget hard cap: heute {spent:.2} USD >= {hard_usd:.2} USD", spent = snap.spent_today_usd
                        ),
                    });
                }
            }
        }
        actions.push(Action::Alert {
            kind: "budget_hard".into(),
            message: format!(
                "Hard-Cap erreicht: heute {:.2} $ verbraucht (Limit {hard_usd:.2} $) — alle Instanzen gestoppt.",
                snap.spent_today_usd
            ),
        });
        return actions;
    }

    let projected_now = snap.projected(0.0, 0.0, 0.0);
    if projected_now >= soft_usd * 0.8 && projected_now < soft_usd {
        actions.push(Action::Alert {
            kind: "budget_80".into(),
            message: format!(
                "Budget-Projektion bis Mitternacht {projected_now:.2} $ erreicht 80 % des Tages-Sof-Caps ({soft_usd:.2} $)."
            ),
        });
    }

    // Fuer Starts/Erhoehungen zaehlt das 30-Minuten-Aktionsfenster.
    let over_soft = snap.spent_today_usd >= soft_usd || snap.projected_30m(0.0, 0.0, 0.0) >= soft_usd;
    let over_monthly = snap.spent_month_usd >= monthly_usd;

    for slot in &snap.slots {
        let scfg = match cfg.slots.get(&slot.id) {
            Some(c) => c,
            None => continue,
        };

        // --- Health-Clamp: keine Automatik auf gepinnten Slots/Instanzen.
        let slot_pinned = slot.pinned;
        let active = slot.active();

        // --- Warmup-Watchdog: Backversuch abbrechen.
        for inst in &slot.instances {
            if inst.state.is_active()
                && inst.state != InstanceState::Healthy
                && inst.state != InstanceState::Draining
                && (snap.now - inst.created_at).num_seconds() > scfg.swap.max_warmup_s
            {
                actions.push(Action::Destroy {
                    instance_id: inst.vast_id,
                    reason: "swap_failed: warmup timeout".into(),
                });
                actions.push(Action::Alert {
                    kind: "swap_failed".into(),
                    message: format!(
                        "Slot {} Instanz {}: nicht healthy nach {} s — destroyed.",
                        slot.id, inst.vast_id, scfg.swap.max_warmup_s
                    ),
                });
            }
        }

        // --- Flip: Replacement healthy → Slot flippen, alten rausschmeißen.
        if let Some(repl) = slot.healthy_replacement() {
            if let Some(old) = active {
                if old.vast_id != repl.vast_id {
                    actions.push(Action::FlipSlot {
                        slot_id: slot.id,
                        from_instance: old.vast_id,
                        to_instance: repl.vast_id,
                        reason: "replacement healthy".into(),
                    });
                    actions.push(Action::SwapOut {
                        instance_id: old.vast_id,
                        destroy: slot.role == Role::Media,
                        reason: "hot swap: replaced".into(),
                    });
                }
            } else {
                actions.push(Action::FlipSlot {
                    slot_id: slot.id,
                    from_instance: 0,
                    to_instance: repl.vast_id,
                    reason: "first healthy instance".into(),
                });
            }
        }

        // --- Wake: Slot soll laufen, aber kein Backer da.
        // Warming-Gate: Existiert bereits eine Instanz in aktivem Zustand
        // (booting/healthy/connecting/draining — auch unabgeflipte), ist ein
        // Backer unterwegs → NICHT erneut mieten. Sonst mietet der Wake-Zweig
        // direkt nach dem Flip (active noch None) eine Zweitbox.
        if slot.desired_running && !slot_pinned {
            let running = active.map(|a| a.is_running()).unwrap_or(false);
            let warming = slot.instances.iter().any(|i| i.state.is_active());
            if !running && !warming {
                if let Some(stopped) = slot
                    .instances
                    .iter()
                    .find(|i| i.state == InstanceState::Stopped && !i.pinned)
                {
                    // Restart-Präferenz: gleiche Instanz starten (Disk warm).
                    if let Some(r) = budget_ok_for_start(snap, cfg, stopped, hours_to_end, soft_usd, monthly_usd) {
                        actions.push(Action::Start {
                            instance_id: stopped.vast_id,
                            reason: r,
                        });
                    }
                } else if let Some(offer) = &slot.candidate_offer {
                    if let Some(action) = plan_create(snap, cfg, slot, offer, over_soft, over_monthly, "wake", None) {
                        actions.push(action);
                    }
                } else {
                    actions.push(Action::Alert {
                        kind: "no_offer".into(),
                        message: format!("Slot {} soll laufen, aber kein Kandidat-Angebot gefunden.", slot.id),
                    });
                }
            }
        }

        if let Some(inst) = active {
            let pinned = inst.pinned || slot_pinned;
            let mut replacement_queued = false;

            // --- Preempted (Trigger A): Replacement nur bei frischem Traffic.
            if inst.state == InstanceState::Preempted {
                let had_traffic = slot
                    .last_traffic
                    .map(|t| (snap.now - t).num_seconds() <= scfg.swap.keep_warm_window_s)
                    .unwrap_or(false);
                if had_traffic && scfg.swap.on_preempt && !pinned {
                    if let Some(offer) = &slot.candidate_offer {
                        if let Some(action) = plan_create(snap, cfg, slot, offer, over_soft, over_monthly, "preempted: replace", Some(inst.rate_usd_h())) {
                            actions.push(action);
                        }
                    } else {
                        actions.push(Action::Alert {
                            kind: "preempted".into(),
                            message: format!(
                                "Slot {} Instanz {} preempted, kein Ersatz-Angebot verfügbar.",
                                slot.id, inst.vast_id
                            ),
                        });
                    }
                } else {
                    actions.push(Action::Alert {
                        kind: "preempted".into(),
                        message: format!(
                            "Slot {} Instanz {} preempted (kein Traffic seit Keep-Warm-Fenster).",
                            slot.id, inst.vast_id
                        ),
                    });
                }
            }

            // --- Gebots-Druck (Trigger B): busy → defend, idle → Replacement.
            if inst.mode == Mode::Interruptible && inst.is_running() && inst.min_bid > inst.bid_usd_h + 1e-9 {
                let can_bid = !over_soft && !over_monthly;
                if inst.busy && scfg.bid.defend_when_busy && can_bid && !pinned {
                    let new_bid = (inst.min_bid * 1.1).min(scfg.bid.ceiling_usd_h);
                    if new_bid >= inst.min_bid && new_bid > inst.bid_usd_h + 1e-9 {
                        actions.push(Action::ChangeBid {
                            instance_id: inst.vast_id,
                            price_usd_h: new_bid,
                            reason: format!(
                                "defend bid: busy, min_bid {:.4} > bid {:.4}",
                                inst.min_bid, inst.bid_usd_h
                            ),
                        });
                    } else {
                        actions.push(Action::Alert {
                            kind: "bid_ceiling".into(),
                            message: format!(
                                "Slot {}: min_bid {:.4} übersteigt Ceiling {:.4} — Job läuft ohne Verteidigung weiter.",
                                slot.id, inst.min_bid, scfg.bid.ceiling_usd_h
                            ),
                        });
                    }
                } else if !inst.busy && scfg.swap.on_bid_pressure && !pinned {
                    if let Some(offer) = &slot.candidate_offer {
                        let swap_recent = slot
                            .last_swap
                            .map(|t| (snap.now - t).num_seconds() < scfg.swap.min_swap_interval_s)
                            .unwrap_or(false);
                        if !swap_recent {
                            if let Some(action) = plan_create(snap, cfg, slot, offer, over_soft, over_monthly, "bid pressure: cheaper replacement", Some(inst.rate_usd_h())) {
                                actions.push(action);
                                replacement_queued = true;
                            }
                        }
                    }
                }
            }

            // --- Kosten-Optimierung (Trigger C, für llm per Default aus).
            if scfg.swap.optimize_cost && !pinned && inst.is_running() {
                let rate = inst.rate_usd_h();
                let swap_recent = slot
                    .last_swap
                    .map(|t| (snap.now - t).num_seconds() < scfg.swap.min_swap_interval_s)
                    .unwrap_or(false);
                if !swap_recent {
                    if let Some(offer) = &slot.candidate_offer {
                        let offer_rate = offer.min_bid.max(0.0);
                        let savings_pct = (1.0 - offer_rate / rate.max(1e-9)) * 100.0;
                        if savings_pct >= scfg.swap.min_savings_pct {
                            if let Some(action) = plan_create(snap, cfg, slot, offer, over_soft, over_monthly, "optimize cost: cheaper offer", Some(inst.rate_usd_h())) {
                                actions.push(action);
                            }
                        }
                    }
                }
            }

            // --- Idle → Stop (lifecycle Auto/Sleep), Pinned ausgenommen.
            // Bei laufendem Ersatz-Swap kein Idle-Stop: die alte Instanz
            // haelt den Slot warm, bis das Replacement healthy ist.
            if !pinned && !replacement_queued && inst.state == InstanceState::Healthy && !inst.busy && slot.in_flight == 0 {
                // Idle-Uhr: letzter Traffic ODER Healthy-Werden — ohne beides
                // (frisch geflippt, noch nie Traffic) läuft sie noch nicht.
                if let Some(idle_base) = inst.idle_since {
                    let idle_secs = (snap.now - idle_base).num_seconds();
                    let stop_after = match &inst.lifecycle {
                        praxis_common::Lifecycle::Sleep { stop_after_idle_s, .. } => *stop_after_idle_s,
                        _ => scfg.idle.stop_after_s,
                    };
                    let over_soft_idle = snap.spent_today_usd >= soft_usd && idle_secs > 0;
                    if idle_secs >= stop_after || over_soft_idle {
                        actions.push(Action::Stop {
                            instance_id: inst.vast_id,
                            reason: if over_soft_idle {
                                format!("idle + budget soft cap: stop sofort ({idle_secs}s idle)")
                            } else {
                                format!("idle seit {idle_secs}s (stop_after {stop_after}s)")
                            },
                        });
                    }
                }
            }
        }

        // --- Preempted-Aufräumen: nie healthy gewordene Boxen sind Müll
        // (Vast hat sie beendet; Storage läuft bis destroy weiter). Healthy
        // gewesene bleiben für Restart-Präferenz/manuelle Entscheidung.
        for inst in &slot.instances {
            if inst.state == InstanceState::Preempted && !inst.healthy && !inst.pinned {
                actions.push(Action::Destroy {
                    instance_id: inst.vast_id,
                    reason: "preempted vor healthy — aufräumen (kein Warmhaltewert)".into(),
                });
            }
        }

        // --- Destroy nach Stoppen (Storage-Kosten).
        for inst in &slot.instances {
            if inst.state == InstanceState::Stopped && !inst.pinned {
                if let Some(since) = inst.stopped_since {
                    let stopped_secs = (snap.now - since).num_seconds();
                    if stopped_secs >= scfg.idle.destroy_after_stopped_s {
                        actions.push(Action::Destroy {
                            instance_id: inst.vast_id,
                            reason: format!(
                                "destroy_after_stopped: {}s >= {}s (Storage sparen)",
                                stopped_secs, scfg.idle.destroy_after_stopped_s
                            ),
                        });
                    }
                }
            }
        }

        // --- Draining abschließen (kein Traffic mehr → stop/destroy).
        for inst in &slot.instances {
            if inst.state == InstanceState::Draining && slot.in_flight == 0 {
                actions.push(Action::SwapOut {
                    instance_id: inst.vast_id,
                    destroy: slot.role == Role::Media,
                    reason: "drain abgeschlossen".into(),
                });
            }
        }

        // --- Lifecycle-Zeitpläne auf ALLE Instanzen des Slots.
        for inst in &slot.instances {
            if inst.pinned || slot_pinned {
                continue;
            }
            match &inst.lifecycle {
                praxis_common::Lifecycle::Until { until, destroy } => {
                    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(until) {
                        if snap.now >= t {
                            lifecycle_stop(&mut actions, inst, *destroy, until.clone());
                        }
                    }
                }
                praxis_common::Lifecycle::Ttl { ttl_s, destroy } => {
                    if (snap.now - inst.created_at).num_seconds() >= *ttl_s {
                        lifecycle_stop(&mut actions, inst, *destroy, format!("ttl {ttl_s}s"));
                    }
                }
                praxis_common::Lifecycle::Schedule { spec, prewarm_s, destroy } => {
                    if let Some(win) = schedule::parse_window(spec) {
                        if win.contains(slot.local_weekday, slot.local_minutes_of_day) {
                            // Im Fenster: soll laufen.
                            if inst.state == InstanceState::Stopped && inst.is_stop_intended() {
                                actions.push(Action::Start {
                                    instance_id: inst.vast_id,
                                    reason: format!("schedule {spec}: Fenster aktiv"),
                                });
                            }
                        } else if let Some(prewarm_min) = win.prewarm_start((*prewarm_s / 60).max(1) as u32, slot.local_weekday) {
                            if inst.state == InstanceState::Stopped
                                && slot.local_minutes_of_day >= prewarm_min
                            {
                                actions.push(Action::Start {
                                    instance_id: inst.vast_id,
                                    reason: format!("schedule {spec}: Prewarm {prewarm_s}s"),
                                });
                            }
                        } else if inst.is_running() {
                            lifecycle_stop(&mut actions, inst, *destroy, format!("schedule {spec}: Fenster zu"));
                        }
                    }
                }
                praxis_common::Lifecycle::Sleep { resume_at, resume_after_s, .. } => {
                    if inst.state == InstanceState::Stopped {
                        let resume = 'res: {
                            if let Some(at) = resume_at {
                                if let Ok(t) = chrono::NaiveTime::parse_from_str(at, "%H:%M") {
                                    break 'res Some(t);
                                }
                            }
                            if let Some(after) = resume_after_s {
                                break 'res Some(
                                    (inst.stopped_since.map(|s| s.time()).unwrap_or_default())
                                        + chrono::Duration::seconds(*after),
                                );
                            }
                            None
                        };
                        if let Some(t) = resume {
                            let now_local = chrono::NaiveTime::from_hms_opt(
                                (slot.local_minutes_of_day / 60) as u32,
                                slot.local_minutes_of_day % 60,
                                0,
                            )
                            .unwrap_or_default();
                            if now_local >= t {
                                actions.push(Action::Start {
                                    instance_id: inst.vast_id,
                                    reason: format!("sleep: resume_at {t}"),
                                });
                            }
                        }
                    }
                }
                praxis_common::Lifecycle::Auto => {}
            }
        }
    }

    actions
}

fn lifecycle_stop(actions: &mut Vec<Action>, inst: &InstanceSnapshot, destroy: bool, why: String) {
    if inst.is_running() {
        if destroy {
            actions.push(Action::Destroy {
                instance_id: inst.vast_id,
                reason: format!("lifecycle ({why})"),
            });
        } else {
            actions.push(Action::Stop {
                instance_id: inst.vast_id,
                reason: format!("lifecycle ({why})"),
            });
        }
    }
}

impl InstanceSnapshot {
    fn is_stop_intended(&self) -> bool {
        self.intended_status == "stopped" || self.actual_status == "stopped"
    }
}

/// Budget-Check für Start einer gestoppten Instanz.
fn budget_ok_for_start(
    snap: &Snapshot,
    cfg: &PolicyConfig,
    inst: &InstanceSnapshot,
    hours_to_end: f64,
    soft_usd: f64,
    monthly_usd: f64,
) -> Option<String> {
    let rate = inst.rate_usd_h();
    let projected = snap.projected_30m(rate, 0.0, 0.0);
    if projected > soft_usd {
        return None;
    }
    if snap.spent_month_usd + rate * hours_to_end > monthly_usd {
        return None;
    }
    let _ = cfg;
    Some(format!(
        "wake: gestoppte Instanz {v} starten (rate {rate:.4} $/h, Projektion {projected:.2} $ ≤ soft {soft_usd:.2} $)",
        v = inst.vast_id
    ))
}

/// Create planen inkl. Limits + Budget. `None` = nicht möglich.
fn plan_create(
    snap: &Snapshot,
    cfg: &PolicyConfig,
    slot: &SlotSnapshot,
    offer: &OfferSnapshot,
    over_soft: bool,
    over_monthly: bool,
    why: &str,
    replaced_rate: Option<f64>,
) -> Option<Action> {
    if over_soft || over_monthly {
        return None;
    }
    let slot_count = slot
        .instances
        .iter()
        // Tote Instanzen belegen keinen Miet-Slot: preempted/failed sind
        // weg (Vast GC't sie ggf. erst später), destroyed erst recht nicht.
        // Sonst blockieren Zombies aus dem Wake-Churn jedes neue Create
        // (beobachtet 2026-09-20: 2 preempted Warmup-Tode stoppten den Loop).
        .filter(|i| {
            !matches!(i.state, InstanceState::Preempted | InstanceState::Failed | InstanceState::Destroyed)
        })
        .count() as i64;
    if slot_count >= cfg.limits.max_per_slot {
        return None;
    }
    if snap.instance_count as i64 >= cfg.limits.max_instances {
        return None;
    }
    let scfg = slot_cfg(cfg, slot);
    let bid = (offer.min_bid * (1.0 + scfg.bid.margin)).min(scfg.bid.ceiling_usd_h);
    if bid < offer.min_bid {
        return Some(Action::Alert {
            kind: "bid_ceiling".into(),
            message: format!(
                "Slot {}: Ceiling {:.4} $/h unter min_bid {:.4} $/h — kein Create.",
                slot.id, scfg.bid.ceiling_usd_h, offer.min_bid
            ),
        });
    }
    let projected = snap.projected_30m(bid, replaced_rate.unwrap_or(0.0), 0.0);
    let soft_usd = cfg.budget.daily_soft_eur * cfg.budget.usd_per_eur;
    let monthly_usd = cfg.budget.monthly_eur * cfg.budget.usd_per_eur;
    let hours_to_end = (snap.seconds_to_day_end.max(0) as f64) / 3600.0;
    if projected > soft_usd || snap.spent_month_usd + bid * hours_to_end > monthly_usd {
        return None;
    }
    if snap.running_rate_usd_h + bid > cfg.limits.max_total_rate_usd_h {
        return Some(Action::Alert {
            kind: "rate_limit".into(),
            message: format!(
                "Slot {}: Create würde max_total_rate {:.2} $/h sprengen (aktuell {:.2} + {bid:.2}).",
                slot.id, cfg.limits.max_total_rate_usd_h, snap.running_rate_usd_h
            ),
        });
    }
    Some(Action::Create {
        slot_id: slot.id,
        offer_id: offer.id,
        mode: Mode::Interruptible,
        price_usd_h: Some(bid),
        disk_gb: None,
        reason: format!("{why}: min_bid {:.4} + margin → bid {bid:.4}", offer.min_bid),
    })
}

fn slot_cfg<'a>(cfg: &'a PolicyConfig, slot: &SlotSnapshot) -> &'a SlotPolicyCfg {
    cfg.slots.get(&slot.id).unwrap_or_else(|| panic!("missing cfg for slot {}", slot.id))
}