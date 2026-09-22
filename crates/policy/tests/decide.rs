//! Zeitachsen-Simulationen für die Policy — der Bauplan 4.x als Unit-Tests.

use chrono::{TimeZone, Utc};
use praxis_common::{Action, InstanceState, Mode, Role};
use praxis_policy::{
    decide, BidConfig, BudgetConfig, IdleConfig, InstanceSnapshot, LimitsConfig, OfferSnapshot,
    PolicyConfig, PoolConfig, SlotMode, SlotPolicyCfg, SlotSnapshot, Snapshot, SwapConfig,
};
use std::collections::HashMap;

#[test]
fn pinned_warmup_and_draining_instances_are_not_removed() {
    for state in [InstanceState::Booting, InstanceState::Draining] {
        let mut a = inst(11, 1, Role::Llm, state);
        a.pinned = true;
        let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
        let actions = decide(&s, &cfg());
        assert!(!actions.iter().any(|a| matches!(a, Action::Destroy {..} | Action::SwapOut {..} | Action::Stop {..})), "{actions:?}");
    }
}

#[test]
fn locked_slot_does_not_flip_or_destroy_on_replacement_readiness() {
    let a = inst(11, 1, Role::Llm, InstanceState::Booting);
    let b = inst(12, 1, Role::Llm, InstanceState::Healthy);
    let mut s = slot(1, Role::Llm, vec![a,b], Some(11));
    s.pinned = true;
    assert!(decide(&snap(vec![s]), &cfg()).is_empty());
}

fn cfg() -> PolicyConfig {
    let mut slots = HashMap::new();
    slots.insert(
        1,
        SlotPolicyCfg {
            bid: BidConfig { margin: 0.15, ceiling_usd_h: 0.30, defend_when_busy: true, rent_min_usd_h: 0.0 },
            idle: IdleConfig { stop_after_s: 900, destroy_after_stopped_s: 172_800 },
            mode: Default::default(),
            swap: SwapConfig {
                on_preempt: true,
                on_bid_pressure: true,
                optimize_cost: false,
                min_savings_pct: 25.0,
                max_warmup_s: 2700,
                min_swap_interval_s: 3600,
                keep_warm_window_s: 1800,
                allow_long_downloads: false,
            },
            pool: Default::default(),
        },
    );
    slots.insert(
        2,
        SlotPolicyCfg {
            bid: BidConfig { margin: 0.10, ceiling_usd_h: 0.20, defend_when_busy: true, rent_min_usd_h: 0.0 },
            idle: IdleConfig { stop_after_s: 600, destroy_after_stopped_s: 3600 },
            mode: Default::default(),
            swap: SwapConfig {
                on_preempt: true,
                on_bid_pressure: true,
                optimize_cost: true,
                min_savings_pct: 25.0,
                max_warmup_s: 1200,
                min_swap_interval_s: 3600,
                keep_warm_window_s: 1800,
                allow_long_downloads: false,
            },
            pool: Default::default(),
        },
    );
    PolicyConfig {
        budget: BudgetConfig { daily_soft_eur: 2.0, daily_hard_eur: 2.4, monthly_eur: 50.0, usd_per_eur: 1.08, hard_action: "stop".into() },
        limits: LimitsConfig { max_instances: 3, max_per_slot: 2, max_total_rate_usd_h: 0.60 },
        slots,
    }
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 21, 12, 0, 0).unwrap()
}

fn inst(vast_id: i64, slot_id: i64, role: Role, state: InstanceState) -> InstanceSnapshot {
    InstanceSnapshot {
        vast_id,
        offer_id: 9000 + vast_id,
        machine_id: 100,
        gpu_name: "RTX 5070 Ti".into(),
        role,
        slot_id,
        mode: Mode::Interruptible,
        lifecycle: praxis_common::Lifecycle::Auto,
        state,
        actual_status: if state.is_active() { "running".into() } else { "stopped".into() },
        intended_status: "running".into(),
        healthy: state == InstanceState::Healthy,
        busy: false,
        busy_reason: String::new(),
        min_bid: 0.10,
        bid_usd_h: 0.115,
        dph_total: 0.20,
        storage_usd_h: 0.005,
        created_at: now() - chrono::Duration::hours(1),
        boot_started_at: now() - chrono::Duration::hours(1),
        resume_at: None,
        idle_since: None,
        stopped_since: None,
        last_seen: Some(now()),
        pinned: false,
    }
}

fn slot(id: i64, role: Role, instances: Vec<InstanceSnapshot>, active: Option<i64>) -> SlotSnapshot {
    SlotSnapshot {
        id,
        role,
        name: format!("slot {id}"),
        pinned: false,
        active_instance: active,
        instances,
        in_flight: 0,
        last_traffic: Some(now() - chrono::Duration::minutes(5)),
        last_swap: None,
        candidate_offer: Some(OfferSnapshot {
            id: 50349013,
            machine_id: 3754,
            gpu_name: "RTX 4060 Ti".into(),
            min_bid: 0.105,
            dph_total: 0.22,
            storage_cost: 0.15,
            inet_down_cost: 0.002,
            cpu_ram_gb: 126.0,
            gpu_ram_gb: 16.0,
            disk_gb: 200.0,
            inet_down: 900.0,
            reliability2: 0.98,
            disk_bw: 2000.0,
            ..Default::default()
        }),
        desired_running: false,
        local_weekday: 1,
        local_minutes_of_day: 12 * 60,
    }
}

fn snap(slots: Vec<SlotSnapshot>) -> Snapshot {
    let instance_count: usize = slots.iter().map(|s| s.instances.len()).sum();
    let running_rate: f64 = slots
        .iter()
        .flat_map(|s| s.instances.iter())
        .filter(|i| i.is_running())
        .map(|i| if i.mode == Mode::Interruptible { i.bid_usd_h } else { i.dph_total })
        .sum();
    let storage: f64 = slots.iter().flat_map(|s| s.instances.iter()).map(|i| i.storage_usd_h).sum();
    Snapshot {
        now: now(),
        seconds_to_day_end: 12 * 3600,
        spent_today_usd: 0.0,
        spent_month_usd: 0.0,
        slots,
        instance_count,
        running_rate_usd_h: running_rate,
        storage_rate_usd_h: storage,
        auto_rent_enabled: true,
    }
}

#[test]
fn auto_rent_off_blocks_wake_create_and_start() {
    // Schalter aus + kalter, gewünschter Slot: KEIN Miete-Create —
    // „wenn ich die GPUs nicht verwende, mietet der Router nichts“.
    let mut s = snap(vec![slot(1, Role::Llm, vec![], None)]).with_auto_rent(false);
    s.slots[0].desired_running = true;
    let actions = decide(&s, &cfg());
    assert!(actions.iter().all(|a| !matches!(a, Action::Create { .. })), "{actions:?}");
}

#[test]
fn auto_rent_off_blocks_preempt_replace_but_keeps_stops() {
    // Schalter aus: preemptete Box wird NICHT ersetzt (kein Create), aber
    // Idle-/Budget-Stops laufen weiter — die sparen ja Geld.
    let a = inst(11, 1, Role::Llm, InstanceState::Preempted);
    let mut b = inst(12, 1, Role::Llm, InstanceState::Healthy);
    b.idle_since = Some(now() - chrono::Duration::seconds(1000));
    let s = snap(vec![slot(1, Role::Llm, vec![a, b], Some(12))]).with_auto_rent(false);
    let actions = decide(&s, &cfg());
    assert!(actions.iter().all(|a| !matches!(a, Action::Create { .. } | Action::Start { .. })), "{actions:?}");
    assert!(actions.iter().any(|x| matches!(x, Action::Stop { instance_id: 12, .. })), "{actions:?}");
}

#[test]
fn auto_rent_on_rent_unchanged() {
    // Kontrolle: Schalter an → Wake-Create bleibt (gewünschtes Verhalten).
    let mut s = snap(vec![slot(1, Role::Llm, vec![], None)]);
    s.slots[0].desired_running = true;
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|a| matches!(a, Action::Create { .. })), "{actions:?}");
}

#[test]
fn healthy_idle_instance_stops_after_stop_after() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    a.idle_since = Some(now() - chrono::Duration::seconds(1000));
    let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Stop { instance_id: 11, .. })), "{actions:?}");
}

#[test]
fn busy_instance_never_stops_for_idle() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    a.idle_since = Some(now() - chrono::Duration::seconds(1000));
    a.busy = true;
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    s.slots[0].in_flight = 2;
    let actions = decide(&s, &cfg());
    assert!(!actions.iter().any(|x| matches!(x, Action::Stop { instance_id: 11, .. })), "{actions:?}");
}

#[test]
fn wake_creates_when_no_instance() {
    let mut s = snap(vec![slot(1, Role::Llm, vec![], None)]);
    s.slots[0].desired_running = true;
    let actions = decide(&s, &cfg());
    match actions.iter().find(|x| matches!(x, Action::Create { slot_id: 1, .. })) {
        Some(Action::Create { offer_id, price_usd_h, .. }) => {
            assert_eq!(*offer_id, 50349013);
            // bid = min_bid * 1.15 = 0.1207
            let p = price_usd_h.unwrap();
            assert!((p - 0.1207).abs() < 1e-3, "{p}");
        }
        other => panic!("kein Create: {other:?} in {actions:?}"),
    }
}

#[test]
fn on_demand_slot_creates_at_dph_total() {
    // on_demand-Modus: Create zum Listenpreis (dph_total), Mode OnDemand,
    // kein Outbid-Risiko (2026-09-20: H200-Schnäppchen 0.0153 $/h war
    // Minuten nach Miete outbid — User wollte eine stabile Box).
    let mut c = cfg();
    c.slots.get_mut(&1).unwrap().mode = SlotMode::OnDemand;
    let mut s = snap(vec![slot(1, Role::Llm, vec![], None)]);
    s.slots[0].desired_running = true;
    let actions = decide(&s, &c);
    match actions.iter().find(|x| matches!(x, Action::Create { slot_id: 1, .. })) {
        Some(Action::Create { mode: Mode::OnDemand, price_usd_h, .. }) => {
            // Preis = dph_total des Kandidaten (0.22), nicht min_bid*1.15
            let p = price_usd_h.unwrap();
            assert!((p - 0.22).abs() < 1e-9, "{p}");
        }
        other => panic!("kein On-Demand-Create: {other:?} in {actions:?}"),
    }
}

#[test]
fn on_demand_slot_never_cost_optimizes() {
    // Kosten-Optimierung darf einen on_demand-Slot nicht zurück in den
    // Interruptible-Churn schicken (billigeres min_bid-Angebot lockt).
    let mut c = cfg();
    c.slots.get_mut(&1).unwrap().mode = SlotMode::OnDemand;
    c.slots.get_mut(&1).unwrap().swap.optimize_cost = true;
    let a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    let actions = decide(&s, &c);
    assert!(!actions.iter().any(|x| matches!(x, Action::Create { .. })), "{actions:?}");
}

#[test]
fn wake_prefers_starting_stopped_instance() {
    let a = inst(11, 1, Role::Llm, InstanceState::Stopped);
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    s.slots[0].desired_running = true;
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Start { instance_id: 11, .. })), "{actions:?}");
    assert!(!actions.iter().any(|x| matches!(x, Action::Create { .. })), "{actions:?}");
}

#[test]
fn preempted_with_recent_traffic_triggers_replacement() {
    let a = inst(11, 1, Role::Llm, InstanceState::Preempted);
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    s.slots[0].last_traffic = Some(now() - chrono::Duration::minutes(10));
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Create { slot_id: 1, .. })), "{actions:?}");
}

#[test]
fn preempted_without_traffic_no_replacement() {
    let a = inst(11, 1, Role::Llm, InstanceState::Preempted);
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    s.slots[0].last_traffic = Some(now() - chrono::Duration::hours(3));
    let actions = decide(&s, &cfg());
    assert!(!actions.iter().any(|x| matches!(x, Action::Create { .. })), "{actions:?}");
    assert!(actions.iter().any(|x| matches!(x, Action::Alert { kind, .. } if kind == "preempted")), "{actions:?}");
}

#[test]
fn bid_pressure_busy_defends_with_10pct() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    a.min_bid = 0.13; // > bid 0.115
    a.busy = true;
    let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    let actions = decide(&s, &cfg());
    match actions.iter().find(|x| matches!(x, Action::ChangeBid { .. })) {
        Some(Action::ChangeBid { price_usd_h, .. }) => assert!((price_usd_h - 0.143).abs() < 1e-6),
        other => panic!("kein ChangeBid: {other:?} in {actions:?}"),
    }
}

#[test]
fn bid_pressure_at_ceiling_alerts_instead() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    a.min_bid = 0.32; // ceiling 0.30 < min_bid → keine Verteidigung moeglich
    a.busy = true;
    let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    let actions = decide(&s, &cfg());
    assert!(!actions.iter().any(|x| matches!(x, Action::ChangeBid { .. })), "{actions:?}");
    assert!(actions.iter().any(|x| matches!(x, Action::Alert { kind, .. } if kind == "bid_ceiling")), "{actions:?}");
}

#[test]
fn bid_pressure_idle_prepares_swap() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    a.min_bid = 0.16; // viel teurer geworden
    let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Create { slot_id: 1, .. })), "{actions:?}");
    assert!(!actions.iter().any(|x| matches!(x, Action::ChangeBid { .. })), "{actions:?}");
}

#[test]
fn replacement_healthy_flips_slot_and_swaps_out_old() {
    let a = inst(11, 2, Role::Media, InstanceState::Healthy);
    let b = inst(12, 2, Role::Media, InstanceState::Healthy);
    let mut s = snap(vec![slot(2, Role::Media, vec![a, b], Some(11))]);
    s.seconds_to_day_end = 4 * 3600; // Ueberlappung zaehlt nur kurz
    let actions = decide(&s, &cfg());
    assert!(
        actions.iter().any(|x| matches!(x, Action::FlipSlot { slot_id: 2, to_instance: 12, .. })),
        "{actions:?}"
    );
    assert!(actions.iter().any(|x| matches!(x, Action::SwapOut { instance_id: 11, destroy: true, .. })), "{actions:?}");
}

#[test]
fn warmup_timeout_destroys_failed_backing() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Booting);
    a.boot_started_at = now() - chrono::Duration::hours(2); // > 45 min max_warmup
    let s = snap(vec![slot(1, Role::Llm, vec![a], None)]);
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Destroy { instance_id: 11, .. })), "{actions:?}");
    assert!(actions.iter().any(|x| matches!(x, Action::Alert { kind, .. } if kind == "swap_failed")), "{actions:?}");
}

#[test]
fn hard_cap_stops_everything_and_alerts() {
    let a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    s.spent_today_usd = 5.0; // hart ist 2.4*1.08 = 2.592 USD
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Stop { instance_id: 11, .. })), "{actions:?}");
    assert!(actions.iter().any(|x| matches!(x, Action::Alert { kind, .. } if kind == "budget_hard")), "{actions:?}");
    assert!(!actions.iter().any(|x| matches!(x, Action::Create { .. })), "{actions:?}");
}

#[test]
fn hard_cap_destroy_mode_destroys_instead_of_stop() {
    let a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    s.spent_today_usd = 5.0; // hart ist 2.4*1.08 = 2.592 USD
    let mut c = cfg();
    c.budget.hard_action = "destroy".into();
    let actions = decide(&s, &c);
    assert!(actions.iter().any(|x| matches!(x, Action::Destroy { instance_id: 11, .. })), "{actions:?}");
    assert!(!actions.iter().any(|x| matches!(x, Action::Stop { instance_id: 11, .. })), "{actions:?}");
    assert!(actions.iter().any(|x| matches!(x, Action::Alert { kind, .. } if kind == "budget_hard")), "{actions:?}");
}

#[test]
fn soft_cap_blocks_creates() {
    let mut s = snap(vec![slot(1, Role::Llm, vec![], None)]);
    s.spent_today_usd = 2.30; // soft = 2.16 USD — schon drüber
    s.slots[0].desired_running = true;
    let actions = decide(&s, &cfg());
    assert!(!actions.iter().any(|x| matches!(x, Action::Create { .. })), "{actions:?}");
}

#[test]
fn media_destroys_after_one_hour_stopped_llm_keeps_48h() {
    let mut m = inst(21, 2, Role::Media, InstanceState::Stopped);
    m.stopped_since = Some(now() - chrono::Duration::hours(2));
    let mut l = inst(11, 1, Role::Llm, InstanceState::Stopped);
    l.stopped_since = Some(now() - chrono::Duration::hours(24));
    let s = snap(vec![
        slot(1, Role::Llm, vec![l], Some(11)),
        slot(2, Role::Media, vec![m], Some(21)),
    ]);
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Destroy { instance_id: 21, .. })), "{actions:?}");
    assert!(!actions.iter().any(|x| matches!(x, Action::Destroy { instance_id: 11, .. })), "{actions:?}");
}

#[test]
fn pinned_never_stops_or_swaps() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    a.idle_since = Some(now() - chrono::Duration::hours(5));
    a.pinned = true;
    let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    let actions = decide(&s, &cfg());
    assert!(!actions.iter().any(|x| matches!(x, Action::Stop { .. } | Action::Destroy { .. })), "{actions:?}");
}

#[test]
fn cost_optimization_only_when_enabled_and_cheap_enough() {
    let mut a = inst(21, 2, Role::Media, InstanceState::Healthy);
    a.bid_usd_h = 0.20; // current price
    a.min_bid = 0.20;
    let mut s = snap(vec![slot(2, Role::Media, vec![a], Some(21))]);
    s.seconds_to_day_end = 4 * 3600;
    // candidate 0.105 → Ersparnis 47.5 % ≥ 25 %
    s.slots[0].candidate_offer = Some(OfferSnapshot {
        id: 999,
        machine_id: 4242,
        gpu_name: "RTX 3090".into(),
        min_bid: 0.105,
        dph_total: 0.25,
        storage_cost: 0.15,
        inet_down_cost: 0.002,
        cpu_ram_gb: 64.0,
        gpu_ram_gb: 24.0,
        disk_gb: 512.0,
        inet_down: 1200.0,
        reliability2: 0.99,
        disk_bw: 3000.0,
        ..Default::default()
    });
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Create { slot_id: 2, offer_id: 999, .. })), "{actions:?}");

    // Hysterese: kürzlicher Swap blockt.
    let mut s2 = s.clone();
    s2.slots[0].last_swap = Some(now() - chrono::Duration::minutes(30));
    let actions2 = decide(&s2, &cfg());
    assert!(!actions2.iter().any(|x| matches!(x, Action::Create { slot_id: 2, .. })), "{actions2:?}");
}

#[test]
fn flip_waits_for_streams_and_busy_on_both_backers() {
    for (in_flight, old_busy, new_busy) in [(1, false, false), (0, true, false), (0, false, true)] {
        let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy);
        let mut b = inst(12, 1, Role::Llm, InstanceState::Healthy);
        a.busy = old_busy; b.busy = new_busy;
        let mut s = slot(1, Role::Llm, vec![a, b], Some(11)); s.in_flight = in_flight;
        let actions = decide(&snap(vec![s]), &cfg());
        assert!(!actions.iter().any(|a| matches!(a, Action::FlipSlot { .. } | Action::SwapOut { .. })), "{actions:?}");
    }
}

#[test]
fn resumed_contract_gets_a_fresh_warmup_deadline() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Booting);
    a.created_at = now() - chrono::Duration::days(3);
    a.boot_started_at = now();
    let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    assert!(!decide(&s, &cfg()).iter().any(|a| matches!(a, Action::Destroy { .. })));
}

#[test]
fn manual_contracts_ignore_lifecycle_automation_but_not_monthly_cap() {
    for state in [InstanceState::Healthy, InstanceState::Booting, InstanceState::Stopped, InstanceState::Preempted] {
        let mut a = inst(11, 1, Role::Llm, state);
        a.mode = Mode::Manual;
        a.idle_since = Some(now() - chrono::Duration::hours(5));
        a.stopped_since = Some(now() - chrono::Duration::days(4));
        a.lifecycle = praxis_common::Lifecycle::Ttl { ttl_s: 1, destroy: true };
        let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
        assert!(!decide(&s, &cfg()).iter().any(|a| matches!(a, Action::Stop { .. } | Action::Destroy { .. } | Action::Start { .. } | Action::ChangeBid { .. })));
    }
    let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy); a.mode = Mode::Manual;
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    s.spent_month_usd = 100.0;
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|a| matches!(a, Action::Stop { reason, .. } if reason.contains("month"))));
    assert!(!actions.iter().any(|a| matches!(a, Action::Start { .. } | Action::Create { .. })));
}

#[test]
fn schedule_stops_after_close_but_not_from_ordinary_idle_during_window() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    a.idle_since = Some(now() - chrono::Duration::hours(2));
    a.lifecycle = praxis_common::Lifecycle::Schedule { spec: "Mo-Fr 09:00-18:00".into(), prewarm_s: 1200, destroy: false };
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    s.slots[0].local_weekday = 1;
    s.slots[0].local_minutes_of_day = 12 * 60;
    assert!(!decide(&s, &cfg()).iter().any(|a| matches!(a, Action::Stop { .. })));
    s.slots[0].local_minutes_of_day = 19 * 60;
    assert!(decide(&s, &cfg()).iter().any(|a| matches!(a, Action::Stop { .. })));
}

#[test]
fn sleep_duration_never_wraps_at_midnight_or_depends_on_local_hour() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Stopped);
    a.lifecycle = praxis_common::Lifecycle::Sleep { stop_after_idle_s: 60, resume_at: None, resume_after_s: Some(26 * 3600) };
    a.stopped_since = Some(now() - chrono::Duration::hours(25));
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    assert!(!decide(&s, &cfg()).iter().any(|a| matches!(a, Action::Start { .. })));
    s.slots[0].instances[0].stopped_since = Some(now() - chrono::Duration::hours(27));
    assert!(decide(&s, &cfg()).iter().any(|a| matches!(a, Action::Start { .. })));
    s.slots[0].instances[0].lifecycle = praxis_common::Lifecycle::Sleep { stop_after_idle_s: 60, resume_at: Some("07:30".into()), resume_after_s: None };
    s.slots[0].instances[0].resume_at = Some(now() + chrono::Duration::hours(1));
    assert!(!decide(&s, &cfg()).iter().any(|a| matches!(a, Action::Start { .. })));
    s.slots[0].instances[0].resume_at = Some(now());
    assert!(decide(&s, &cfg()).iter().any(|a| matches!(a, Action::Start { .. })));
}

#[test]
fn scheduled_weekend_disk_is_not_garbage_collected_before_monday_resume() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Stopped);
    a.stopped_since = Some(now() - chrono::Duration::days(3));
    a.lifecycle = praxis_common::Lifecycle::Schedule { spec: "Mo-Fr 09:00-18:00".into(), prewarm_s: 1200, destroy: false };
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]); s.slots[0].local_weekday = 7;
    assert!(!decide(&s, &cfg()).iter().any(|a| matches!(a, Action::Destroy { .. })));
}

#[test]
fn ttl_expiry_stops() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    a.lifecycle = praxis_common::Lifecycle::Ttl { ttl_s: 3600, destroy: false };
    a.created_at = now() - chrono::Duration::hours(2);
    let s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Stop { instance_id: 11, .. })), "{actions:?}");
}

#[test]
fn schedule_start_and_stop() {
    let mut a = inst(11, 1, Role::Llm, InstanceState::Stopped);
    a.lifecycle = praxis_common::Lifecycle::Schedule {
        spec: "Mo-Fr 09:00-18:00".into(),
        prewarm_s: 1200,
        destroy: false,
    };
    // Montag 12:00 → Fenster aktiv → Start.
    let lc = a.lifecycle.clone();
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(11))]);
    s.slots[0].local_weekday = 1;
    s.slots[0].local_minutes_of_day = 12 * 60;
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Start { instance_id: 11, .. })), "{actions:?}");

    // Sonntag → Fenster zu, Instanz läuft → Stop.
    let mut b = inst(11, 1, Role::Llm, InstanceState::Healthy);
    b.lifecycle = lc.clone();
    let mut s2 = snap(vec![slot(1, Role::Llm, vec![b], Some(11))]);
    s2.slots[0].local_weekday = 7;
    let actions2 = decide(&s2, &cfg());
    assert!(actions2.iter().any(|x| matches!(x, Action::Stop { instance_id: 11, .. })), "{actions2:?}");

    // Prewarm 08:40 (Start 09:00 - 20 min).
    let mut a3 = inst(11, 1, Role::Llm, InstanceState::Stopped);
    a3.lifecycle = lc;
    let mut s3 = snap(vec![slot(1, Role::Llm, vec![a3], Some(11))]);
    s3.slots[0].local_weekday = 1;
    s3.slots[0].local_minutes_of_day = 8 * 60 + 40;
    let actions3 = decide(&s3, &cfg());
    assert!(actions3.iter().any(|x| matches!(x, Action::Start { instance_id: 11, .. })), "{actions3:?}");
}

#[test]
fn limits_block_create() {
    let mut c = cfg();
    c.limits.max_instances = 1;
    let mut s = snap(vec![slot(1, Role::Llm, vec![], None)]);
    s.slots[0].desired_running = true;
    s.instance_count = 1;
    let actions = decide(&s, &c);
    assert!(!actions.iter().any(|x| matches!(x, Action::Create { .. })), "{actions:?}");
}

#[test]
fn offer_score_prefers_cheap_rate_with_storage() {
    let o1 = OfferSnapshot {
        id: 1, machine_id: 0, gpu_name: "A".into(), min_bid: 0.10, dph_total: 0.0,
        storage_cost: 0.30, inet_down_cost: 0.005,
        ..Default::default()
    };
    let o2 = OfferSnapshot {
        id: 2, machine_id: 0, gpu_name: "B".into(), min_bid: 0.11, dph_total: 0.0,
        storage_cost: 0.05, inet_down_cost: 0.001,
        ..Default::default()
    };
    // 120 GB Disk, 20 GB Traffic, 4 h erwartet:
    assert!(o2.score(120, 20.0, 4.0) < o1.score(120, 20.0, 4.0));
}

#[test]
fn offer_score_weights_reliability2() {
    // Spike-Lektion: billigster Host (0.95) stirbt reihenweise, solider
    // Nachbar (0.99) kostet 4 % mehr — effektive Kosten drehen das um.
    let flaky = OfferSnapshot {
        id: 1, machine_id: 10, gpu_name: "3090 wackelig".into(), min_bid: 0.100, dph_total: 0.0,
        storage_cost: 0.0, inet_down_cost: 0.0,
        reliability2: 0.95, ..Default::default()
    };
    let solid = OfferSnapshot {
        id: 2, machine_id: 11, gpu_name: "3090 solide".into(), min_bid: 0.104, dph_total: 0.0,
        storage_cost: 0.0, inet_down_cost: 0.0,
        reliability2: 0.99, ..Default::default()
    };
    // flaky: 0.100/0.95 = 0.10526 — solid: 0.104/0.99 = 0.10505 → solid gewinnt.
    assert!(solid.score(60, 0.0, 4.0) < flaky.score(60, 0.0, 4.0));
    // Fehlende reliability2 (0.0) → neutral 0.95, kein absurd hoher Score.
    let unknown = OfferSnapshot { reliability2: 0.0, ..flaky.clone() };
    assert!((unknown.score(60, 0.0, 4.0) - 0.100 / 0.95).abs() < 1e-9);
    // Extreme untere Klemme: 0.5 halbiert nicht den Score ins Bodenlose.
    let terrible = OfferSnapshot { reliability2: 0.1, ..flaky.clone() };
    assert!((terrible.score(60, 0.0, 4.0) - 0.100 / 0.5).abs() < 1e-9);
}
#[test]
fn preempted_warmup_death_is_destroyed_and_frees_slot_quota() {
    // Wake-Churn 2026-09-20: zwei preempted Warmup-Todes blockierten via
    // max_per_slot jedes neue Create. Erwartung: aufräumen + neu mieten.
    let mut z1 = inst(31, 1, Role::Llm, InstanceState::Preempted);
    z1.healthy = false;
    z1.actual_status = "exited".into();
    let mut z2 = inst(32, 1, Role::Llm, InstanceState::Preempted);
    z2.healthy = false;
    z2.actual_status = "exited".into();
    let mut s = snap(vec![slot(1, Role::Llm, vec![z1, z2], None)]);
    s.slots[0].desired_running = true;
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Destroy { instance_id: 31, .. })), "{actions:?}");
    assert!(actions.iter().any(|x| matches!(x, Action::Create { slot_id: 1, .. })), "{actions:?}");
}

#[test]
fn preempted_after_healthy_is_kept() {
    // Healthy-gewesene Box wird NICHT aufgeräumt (Restart-Präferenz).
    let mut h = inst(33, 1, Role::Llm, InstanceState::Preempted);
    h.healthy = true;
    let s = snap(vec![slot(1, Role::Llm, vec![h], Some(33))]);
    let actions = decide(&s, &cfg());
    assert!(!actions.iter().any(|x| matches!(x, Action::Destroy { .. })), "{actions:?}");
}

#[test]
fn fresh_healthy_box_is_not_instantly_stopped_after_long_warmup() {
    // Regression 2026-09-20: Box mit 50-min-Warmup wurde 29 s nach dem Flip
    // gestoppt — Idle zählte ab Creation statt ab Healthy. Erwartung:
    // healthy ohne idle_since (nie Traffic) → NICHT stoppen.
    let a = inst(41, 1, Role::Llm, InstanceState::Healthy);
    let mut s = snap(vec![slot(1, Role::Llm, vec![a], Some(41))]);
    s.slots[0].instances[0].idle_since = None; // frisch geflippt, kein Traffic
    let actions = decide(&s, &cfg());
    assert!(!actions.iter().any(|x| matches!(x, Action::Stop { .. })), "{actions:?}");

    // Nach stop_after ab Healthy → Stop ist korrekt.
    let mut s2 = s.clone();
    s2.slots[0].instances[0].idle_since = Some(now() - chrono::Duration::seconds(1000));
    let actions2 = decide(&s2, &cfg());
    assert!(actions2.iter().any(|x| matches!(x, Action::Stop { instance_id: 41, .. })), "{actions2:?}");
}

#[test]
fn fresh_box_after_midnight_is_not_hard_stopped() {
    // Regressions-Test: frisch gemietete 0.12 $/h-Box kurz nach Mitternacht.
    // Alte Semantik rechnete bis Mitternacht (23.6 h × 0.12 = 2.83 ≥ 2.59
    // hard) und stoppte die Box sofort. Neu: hard = akkumuliert ≥ hard.
    let mut a = inst(21, 2, Role::Media, InstanceState::Healthy);
    a.created_at = now(); // gerade erst gemietet
    let mut s = snap(vec![slot(2, Role::Media, vec![a], Some(21))]);
    s.seconds_to_day_end = 23 * 3600 + 40 * 60; // 00:20 Berlin
    s.spent_today_usd = 0.04; // gerade erst gemietet
    s.running_rate_usd_h = 0.12;
    let actions = decide(&s, &cfg());
    assert!(
        !actions
            .iter()
            .any(|x| match x {
                Action::Stop { .. } => true,
                Action::Alert { kind, .. } if kind == "budget_hard" => true,
                _ => false,
            }),
        "{actions:?}"
    );
}

#[test]
fn hard_cap_fires_on_accumulated_spend_only() {
    let a = inst(21, 2, Role::Media, InstanceState::Healthy);
    let mut s = snap(vec![slot(2, Role::Media, vec![a], Some(21))]);
    s.seconds_to_day_end = 23 * 3600;
    s.spent_today_usd = 2.80; // > 2.592 hard
    s.running_rate_usd_h = 0.12;
    let actions = decide(&s, &cfg());
    assert!(actions.iter().any(|x| matches!(x, Action::Stop { instance_id: 21, .. })), "{actions:?}");
    assert!(actions.iter().any(|x| matches!(x, Action::Alert { kind, .. } if kind == "budget_hard")), "{actions:?}");
}

// ---------------------------------------------------------------- Pool (Multi-Instanz-Slots)

fn pool_cfg() -> PolicyConfig {
    let mut c = cfg();
    c.slots.get_mut(&1).unwrap().pool = PoolConfig { warm: 2, total: 3 };
    c.limits.max_instances = 5;
    c.limits.max_per_slot = 3;
    c.limits.max_total_rate_usd_h = 1.0;
    c
}

/// Slot an (desired), 1 healthy aktiv → Policy mietet die zweite Pool-Box.
#[test]
fn pool_fills_up_to_warm() {
    let a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    let mut s = slot(1, Role::Llm, vec![a], Some(11));
    s.desired_running = true;
    let mut s = snap(vec![s]);
    s.seconds_to_day_end = 23 * 3600;
    s.spent_today_usd = 0.10;
    s.running_rate_usd_h = 0.12;
    let actions = decide(&s, &pool_cfg());
    assert!(
        actions.iter().any(|x| matches!(x, Action::Create { reason, .. } if reason.contains("pool fill"))),
        "pool fill erwartet: {actions:?}"
    );
}

/// 2 healthy (Aufskalierung fertig) → KEIN SwapOut der ersten Box —
/// Koexistenz, Routing verteilt.
#[test]
fn pool_healthy_boxes_coexist_no_swapout() {
    let a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    let b = inst(12, 1, Role::Llm, InstanceState::Healthy);
    let mut s = slot(1, Role::Llm, vec![a, b], Some(11));
    s.desired_running = true;
    let mut s = snap(vec![s]);
    s.seconds_to_day_end = 23 * 3600;
    s.spent_today_usd = 0.10;
    s.running_rate_usd_h = 0.24;
    let actions = decide(&s, &pool_cfg());
    assert!(
        !actions.iter().any(|x| matches!(x, Action::SwapOut { instance_id: 11 | 12, .. })),
        "Koexistenz: keine Box darf rausgeworfen werden: {actions:?}"
    );
    // Auch kein weiterer Create (warm = 2 erreicht).
    assert!(
        !actions.iter().any(|x| matches!(x, Action::Create { .. })),
        "warm erreicht — kein weiterer Create: {actions:?}"
    );
}

/// Aktive Box preempted, zweite healthy → Flip auf die zweite (Failover),
/// und Pool-Füllung mietet/startert Ersatz bis warm wieder 2.
#[test]
fn pool_failover_flips_and_refills() {
    let a = inst(11, 1, Role::Llm, InstanceState::Preempted);
    let b = inst(12, 1, Role::Llm, InstanceState::Healthy);
    let mut s = slot(1, Role::Llm, vec![a, b], Some(11));
    s.desired_running = true;
    let mut s = snap(vec![s]);
    s.seconds_to_day_end = 23 * 3600;
    s.spent_today_usd = 0.10;
    s.running_rate_usd_h = 0.12;
    let actions = decide(&s, &pool_cfg());
    assert!(
        actions.iter().any(|x| matches!(x, Action::FlipSlot { to_instance: 12, .. })),
        "Failover-Flip auf 12 erwartet: {actions:?}"
    );
    assert!(
        actions.iter().any(|x| matches!(x, Action::Create { reason, .. } if reason.contains("pool fill"))),
        "Nachfüllen bis warm=2 erwartet: {actions:?}"
    );
}

/// Gestoppte Reserve (cold standby) wird gestartet statt neu gemietet —
/// Disk warm, kein Image-Pull. Und destroy_after_stopped greift NICHT,
/// solange der Slot gewünscht ist (Reserve-Schutz).
#[test]
fn pool_prefers_cold_reserve_start_over_create() {
    let a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    let mut r = inst(13, 1, Role::Llm, InstanceState::Stopped);
    r.stopped_since = Some(now() - chrono::Duration::hours(72)); // weit über destroy_after
    let mut s = slot(1, Role::Llm, vec![a, r], Some(11));
    s.desired_running = true;
    let mut s = snap(vec![s]);
    s.seconds_to_day_end = 23 * 3600;
    s.spent_today_usd = 0.10;
    s.running_rate_usd_h = 0.12;
    let actions = decide(&s, &pool_cfg());
    assert!(
        actions.iter().any(|x| matches!(x, Action::Start { instance_id: 13, .. })),
        "Reserve 13 starten statt neu mieten: {actions:?}"
    );
    assert!(
        !actions.iter().any(|x| matches!(x, Action::Create { .. })),
        "Create nur wenn keine Reserve: {actions:?}"
    );
    assert!(
        !actions.iter().any(|x| matches!(x, Action::Destroy { instance_id: 13, .. })),
        "Reserve darf nicht zerstört werden, solange der Slot gewünscht ist: {actions:?}"
    );
}

/// Slot aus (desired false) → normale destroy_after_stopped-Regeln
/// (Scale-to-Zero bleibt erhalten, Pool-Schutz fällt weg).
#[test]
fn pool_reserve_unprotected_when_slot_off() {
    let mut r = inst(13, 1, Role::Llm, InstanceState::Stopped);
    r.stopped_since = Some(now() - chrono::Duration::hours(72));
    let mut s = slot(1, Role::Llm, vec![r], None);
    s.desired_running = false;
    let mut s = snap(vec![s]);
    s.seconds_to_day_end = 23 * 3600;
    s.spent_today_usd = 0.10;
    let actions = decide(&s, &pool_cfg());
    assert!(
        actions.iter().any(|x| matches!(x, Action::Destroy { instance_id: 13, .. })),
        "Slot aus → destroy_after_stopped greift: {actions:?}"
    );
}

/// total-Obergrenze: 3 live (1 healthy + 1 preempted + 1 stopped), warm=2 →
/// Reserve starten JA, weitere Create-Box NEIN (3 = total erreicht).
#[test]
fn pool_respects_total_cap() {
    let a = inst(11, 1, Role::Llm, InstanceState::Healthy);
    let p = inst(12, 1, Role::Llm, InstanceState::Preempted);
    let r = inst(13, 1, Role::Llm, InstanceState::Stopped);
    let mut s = slot(1, Role::Llm, vec![a, p, r], Some(11));
    s.desired_running = true;
    let mut s = snap(vec![s]);
    s.seconds_to_day_end = 23 * 3600;
    s.spent_today_usd = 0.10;
    s.running_rate_usd_h = 0.12;
    let actions = decide(&s, &pool_cfg());
    assert!(
        actions.iter().any(|x| matches!(x, Action::Start { instance_id: 13, .. })),
        "Reserve 13 starten: {actions:?}"
    );
    assert!(
        !actions.iter().any(|x| matches!(x, Action::Create { .. })),
        "total=3 erreicht (11+12+13 live) — kein Create: {actions:?}"
    );
}
