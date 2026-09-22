//! Provider capacity waiting is distinct from boot health and real preemption.
use crate::db::InstanceRow;
use praxis_common::InstanceState;

pub(super) fn observed_phase(row:&InstanceRow,provider:&praxis_vast::Instance)->Option<&'static str> {
    if row.intended_status!="running" || row.destroyed_at.is_some() {return None;}
    if provider.waiting_for_capacity() {return Some("scheduling");}
    if row.phase().awaiting_allocation() {
        if provider.allocation_running() {return Some("booting");}
        if matches!(provider.actual_status.as_deref(),Some("error"|"exited")) {return Some("start_failed");}
        if provider.actual_status.as_deref()==Some("running") && provider.cur_state.as_deref()==Some("stopped") {
            return Some("start_requested"); // conflicting observations, not a confirmed queue or allocation
        }
        if provider.actual_status.as_deref()==Some("stopped") || provider.cur_state.as_deref()==Some("stopped") {
            return Some("scheduling");
        }
        if matches!(provider.actual_status.as_deref(),Some("loading"|"created"|"provisioning")) {
            return Some("provisioning");
        }
        // Unknown status is not proof of a failed boot. Preserve the disk.
        return None;
    }
    // Recover an older falsely preempted row only on positive allocation evidence.
    if row.phase()==InstanceState::Preempted && provider.allocation_running() {return Some("booting");}
    None
}

pub(super) fn silence_since_boot(boot:Option<chrono::DateTime<chrono::Utc>>,seen:Option<i64>,now:i64)->i64 {
    // A pre-stop last_seen is not liveness evidence for this allocation attempt.
    let baseline=seen.into_iter().chain(boot.map(|b|b.timestamp())).max().unwrap_or(now);
    now.saturating_sub(baseline).max(0)
}
