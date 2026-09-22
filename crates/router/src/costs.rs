//! Shared current/reserved hourly costs for admission, HTML and notifications.
use crate::db::InstanceRow;

#[derive(Debug, Clone, Copy, Default)]
pub struct Hourly {
    pub compute: f64,
    pub storage: f64,
}
impl Hourly {
    pub fn total(self) -> f64 {
        self.compute + self.storage
    }
}

pub fn hourly<'a>(rows: impl IntoIterator<Item = &'a InstanceRow>) -> anyhow::Result<Hourly> {
    let mut result = Hourly::default();
    for row in rows {
        if row.destroyed_at.is_some() {
            continue;
        }
        let active = row.actual_status == "running"
            || matches!(
                row.state.as_str(),
                "requested" | "provisioning" | "booting" | "agent_connected"
            );
        let compute = if active { row.compute_usd_h() } else { 0.0 };
        anyhow::ensure!(
            compute.is_finite()
                && (if active {
                    compute > 0.0
                } else {
                    compute == 0.0
                })
                && row.storage_usd_h.is_finite()
                && row.storage_usd_h >= 0.0,
            "invalid persisted hourly costs"
        );
        result.compute += compute;
        result.storage += row.storage_usd_h;
    }
    anyhow::ensure!(result.total().is_finite(), "hourly cost overflow");
    Ok(result)
}
