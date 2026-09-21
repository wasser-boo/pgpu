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
    /// Preisfenster (USD/h) für Neumieten: Untergrenze — Angebote darunter
    /// (trotz Suchfilter) sind verdächtige Faker/brechen beim Mieten. 0 = aus.
    #[serde(default)]
    pub rent_min_usd_h: f64,
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
    /// Große Model-Downloads (85-GB-LLM, Qwen3-TTS): Instanzen in der
    /// Download-/Warmup-Phase NICHT über die Automatik zerstören — weder
    /// Warmup-Timeout noch "preempted vor healthy"-Aufräumen. Ein hängender
    /// Download fällt über agent-unreachable/instance_gone weiter auf.
    #[serde(default)]
    pub allow_long_downloads: bool,
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
    /// Miet-Modus: interruptible (Schnäppchen, outbid-bar) oder
    /// on_demand (stabil, dph_total als Preis).
    #[serde(default)]
    pub mode: SlotMode,
    /// Instanz-Pool: warm = gleichzeitig laufende Boxen (wenn Slot an),
    /// total = Obergrenze inkl. kalt gestoppter Reserve. Default 1/1 =
    /// klassisches Einzel-Box-Verhalten.
    #[serde(default)]
    pub pool: PoolConfig,
}

/// Multi-Instanz-Pool pro Slot ("3 running" / "2 warm 1 cold" / …):
/// - warm: Ziel-Anzahl aktiver (booting..healthy) Instanzen, solange der
///   Slot gewünscht ist. Fällt eine Box aus (preempt/host-tot), füllt die
///   Policy automatisch nach — zuerst Reserve starten (Disk warm!), dann
///   neu mieten.
/// - total: Obergrenze lebender Instanzen des Slots. Gestoppte Instanzen
///   unterhalb total sind geschützte Reserve (kein destroy_after_stopped,
///   solange der Slot gewünscht ist).
/// - Routing verteilt Requests über alle healthy Instanzen (Router-seitig,
///   Round-Robin + Job-Stickiness) — mehrere Chats parallel möglich.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct PoolConfig {
    #[serde(default = "d_pool_one")]
    pub warm: usize,
    #[serde(default = "d_pool_one")]
    pub total: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self { warm: 1, total: 1 }
    }
}

fn d_pool_one() -> usize {
    1
}

/// Slot-weiter Mietmodus. `on_demand` mietet zum listenpreis (dph_total):
/// kein Outbid-Risiko — für stabile Tests/Produktion statt Churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SlotMode {
    #[default]
    Interruptible,
    OnDemand,
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
    /// Auto-Miete-Schalter (Dashboard/API): `false` = der Router mietet und
    /// startet NIE automatisch (keine Wake-/Replace-/Schnäppchen-Miete).
    /// Stop/Destroy/Idle laufen weiter — die sparen Geld. Manuelle API-Aktionen
    /// des Nutzers bleiben erlaubt (der Schalter heißt „auto“, nicht „alles“).
    pub auto_rent_enabled: bool,
}

impl Snapshot {
    /// Test-/Reconciler-Helfer: Auto-Miete-Schalter setzen.
    pub fn with_auto_rent(mut self, enabled: bool) -> Self {
        self.auto_rent_enabled = enabled;
        self
    }

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
            if scfg.swap.allow_long_downloads {
                // Bewusst: keine Warmup-Zeitbombe bei großen Downloads.
                continue;
            }
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
        // Pool: Bei mehreren gewünschten warmen Instanzen koexistieren
        // healthy Boxen — Flip+SwapOut nur, wenn die AKTIVE krank ist
        // (Failover) oder klassischer Einzel-Swap (pool.warm <= 1, das
        // Replacement wurde gezielt als Ersatz gemietet).
        if let Some(repl) = slot.healthy_replacement() {
            if let Some(old) = active {
                if old.vast_id != repl.vast_id {
                    let old_healthy = old.state == InstanceState::Healthy;
                    if !old_healthy || scfg.pool.warm <= 1 {
                        actions.push(Action::FlipSlot {
                            slot_id: slot.id,
                            from_instance: old.vast_id,
                            to_instance: repl.vast_id,
                            reason: if old_healthy {
                                "replacement healthy".into()
                            } else {
                                format!("failover: aktive Box {} ({:?}) ersetzt", old.vast_id, old.state)
                            },
                        });
                        actions.push(Action::SwapOut {
                            instance_id: old.vast_id,
                            destroy: slot.role == Role::Media,
                            reason: "hot swap: replaced".into(),
                        });
                    }
                    // Pool-Aufskalierung (warm > 1, aktive healthy): beide
                    // laufen weiter — Routing verteilt den Traffic.
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

        // --- Pool-Füllstand (verallgemeinertes Wake): Slot soll laufen →
        // pool.warm aktive Instanzen (booting..healthy). Fällt eine aus
        // (preempt/unreachable/destroyed), füllt der nächste Tick nach:
        // zuerst Reserve starten (Disk warm, kein Image-Pull), sonst neu
        // mieten bis pool.total. Warming-Gate: bereits aktive (auch
        // ungeflippte) zählen — kein Doppel-Mieten.
        if slot.desired_running && !slot_pinned {
            let actives = slot.instances.iter().filter(|i| i.state.is_active()).count();
            // Live-Zähler fürs total-Limit: Preempted (nie-healthy-Zombies UND
            // ausgebene Boxen) zählen nicht — sie werden geräumt bzw. als
            // Restart-Kandidat behandelt und dürfen die Pool-Grenze nicht
            // blockieren (sonst: Box preempted → kein Refill → für immer
            // unter-warm). Failed (Warmup-Müll) ebenso.
            let live = slot
                .instances
                .iter()
                .filter(|i| !matches!(i.state, InstanceState::Preempted | InstanceState::Failed))
                .count();
            if actives < scfg.pool.warm {
                // Restart-Präferenz: GESTOPPTE Box starten (Disk warm, kein
                // Image-Pull). Preempted bewusst NICHT starten: Restart am
                // gleichen Bid wäre sofort wieder ausgeboben — der Restart-
                // Kandidat wird über SwapOut→stopped erreichbar.
                if let Some(cand) = slot
                    .instances
                    .iter()
                    .find(|i| i.state == InstanceState::Stopped && !i.pinned)
                {
                    if let Some(r) = budget_ok_for_start(snap, cfg, cand, hours_to_end, soft_usd, monthly_usd) {
                        actions.push(Action::Start {
                            instance_id: cand.vast_id,
                            reason: if scfg.pool.warm > 1 {
                                format!("{} — Pool auffüllen ({actives}/{} warm)", r, scfg.pool.warm)
                            } else {
                                r
                            },
                        });
                    }
                } else if live < scfg.pool.total {
                    if let Some(offer) = &slot.candidate_offer {
                        if let Some(action) = plan_create(snap, cfg, slot, offer, over_soft, over_monthly, "pool fill", None) {
                            actions.push(action);
                        }
                    } else {
                        actions.push(Action::Alert {
                            kind: "no_offer".into(),
                            message: format!("Slot {} soll laufen (Pool {}/{}), aber kein Kandidat-Angebot gefunden.", slot.id, actives, scfg.pool.warm),
                        });
                    }
                }
            }
        }

        if let Some(inst) = active {
            let pinned = inst.pinned || slot_pinned;
            let mut replacement_queued = false;

            // --- Preempted (Trigger A): Replacement nur bei frischem Traffic.
            // Pool-Slots (warm > 1): die Pool-Füllung above übernimmt das
            // Nachfüllen (Reserve-Start vor Neumiete) — hier KEIN zweiter
            // Create, sonst Doppel-Miete.
            if inst.state == InstanceState::Preempted && scfg.pool.warm <= 1 {
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
            // Pool-Slots (warm > 1) mieten bewusst stabil — Churn-Swaps
            // würden die Pool-Füll-Logik durcheinanderbringen (eine neue
            // healthy Box ist dort Aufskalierung, kein Ersatz).
            if inst.mode == Mode::Interruptible && inst.is_running() && inst.min_bid > inst.bid_usd_h + 1e-9 && scfg.pool.warm <= 1 {
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
            // On-Demand-Slots mieten bewusst stabil — kein Churn zurück
            // in den Interruptible-Markt. Pool-Slots ebenso (s. Trigger B).
            if scfg.swap.optimize_cost && scfg.mode != SlotMode::OnDemand && scfg.pool.warm <= 1 && !pinned && inst.is_running() {
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
                // Download-Boxen weiter stehen lassen, wenn gewünscht
                // (Disk+Warmup bleiben erhalten — Restart statt Neumiete).
                if scfg.swap.allow_long_downloads {
                    continue;
                }
                actions.push(Action::Destroy {
                    instance_id: inst.vast_id,
                    reason: "preempted vor healthy — aufräumen (kein Warmhaltewert)".into(),
                });
            }
        }

        // --- Pool: ausgebene Boxen (preempted, healthy gewesen) nach dem
        // Keep-Warm-Fenster räumen — Restart am gleichen Bid wäre sofort
        // wieder ausgeboben, die Pool-Füllung mietet derweil frisch; ohne
        // Traffic läuft die Box sonst Storage-Kosten ohne Nutzen.
        if scfg.pool.total > 1 {
            for inst in &slot.instances {
                if inst.state != InstanceState::Preempted || !inst.healthy || inst.pinned {
                    continue;
                }
                let stale = slot
                    .last_traffic
                    .map(|t| (snap.now - t).num_seconds() > scfg.swap.keep_warm_window_s)
                    .unwrap_or(true);
                if stale {
                    actions.push(Action::Destroy {
                        instance_id: inst.vast_id,
                        reason: format!(
                            "preempted nach healthy, kein Traffic seit Keep-Warm-Fenster (Pool räumt auf)"
                        ),
                    });
                }
            }
        }

        // --- Destroy nach Stoppen (Storage-Kosten).
        // Pool-Reserve: Gestoppte Instanzen unterhalb pool.total bleiben
        // erhalten, solange der Slot gewünscht ist — sie sind die KALTEN
        // Standbys ("1 warm 2 cold") für schnelles Failover ohne Image-
        // Pull. Ohne Slot-Bedarf greifen die normalen destroy-Regeln
        // (Scale-to-Zero bleibt erhalten).
        let live = slot
            .instances
            .iter()
            .filter(|i| !(i.state == InstanceState::Preempted && !i.healthy))
            .count();
        let pool_reserve = slot.desired_running && scfg.pool.total > 1 && live <= scfg.pool.total;
        for inst in &slot.instances {
            if inst.state == InstanceState::Stopped && !inst.pinned && !pool_reserve {
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

    // Auto-Miete-Aus (Dashboard-Schalter): keine Creates/Starts mehr — weder
    // Wake-Ersatz noch Preempt-Replace noch Schnäppchen-Optimierung. Stop/
    // Destroy/Idle/Budget laufen weiter (die reduzieren Kosten). Der
    // Toggle-Handler stoppt laufende Boxen zusätzlich sofort.
    if !snap.auto_rent_enabled {
        actions.retain(|a| {
            !matches!(a, Action::Create { .. } | Action::Start { .. })
        });
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
    // Miet-Modus: on_demand zahlt Listenpreis (dph_total) und ist outbid-sicher;
    // interruptible biedet min_bid + Margin (Schnäppchen mit Verdrängungsrisiko).
    let (mode, price) = match scfg.mode {
        SlotMode::OnDemand => {
            let dph = offer.dph_total;
            if dph <= 0.0 {
                return None;
            }
            if dph > scfg.bid.ceiling_usd_h {
                return Some(Action::Alert {
                    kind: "bid_ceiling".into(),
                    message: format!(
                        "Slot {}: On-Demand dph {:.4} über Ceiling {:.4} $/h — kein Create.",
                        slot.id, dph, scfg.bid.ceiling_usd_h
                    ),
                });
            }
            (Mode::OnDemand, dph)
        }
        SlotMode::Interruptible => {
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
            (Mode::Interruptible, bid)
        }
    };
    let bid = price;
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
        mode,
        price_usd_h: Some(bid),
        disk_gb: None,
        reason: match mode {
            Mode::OnDemand => format!("{why}: on-demand dph {bid:.4}"),
            _ => format!("{why}: min_bid {:.4} + margin → bid {bid:.4}", offer.min_bid),
        },
    })
}

fn slot_cfg<'a>(cfg: &'a PolicyConfig, slot: &SlotSnapshot) -> &'a SlotPolicyCfg {
    cfg.slots.get(&slot.id).unwrap_or_else(|| panic!("missing cfg for slot {}", slot.id))
}