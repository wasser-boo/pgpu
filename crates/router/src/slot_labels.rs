//! Canonical Vast slot labels, shared by creation, billing scope and peer protection.
use crate::config::Config;
use praxis_common::Role;

pub struct Label {
    pub role: Role,
    pub slot_id: i64,
    pub prefix: String,
}
impl Label {
    pub fn parse(label: &str) -> Option<Self> {
        let parts: Vec<_> = label.split('-').collect();
        if parts.len() != 4 || parts[0] != "praxis" {
            return None;
        }
        let role = match parts[1] {
            "llm" => Role::Llm,
            "media" => Role::Media,
            _ => return None,
        };
        let slot_id: i64 = parts[2].strip_prefix('s')?.parse().ok()?;
        if slot_id <= 0
            || format!("s{slot_id}") != parts[2]
            || parts[3].len() != 8
            || !parts[3].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        Some(Self {
            role,
            slot_id,
            prefix: parts[3].to_ascii_lowercase(),
        })
    }
    pub fn hostname(&self) -> String {
        format!("gpu-{}-{}", self.role, self.prefix)
    }
}
pub fn create(role: Role, slot_id: i64, token_prefix: &str) -> String {
    format!("praxis-{role}-s{slot_id}-{token_prefix}")
}
pub fn matching_slot(label: &str, cfg: &Config) -> Option<i64> {
    let label = Label::parse(label)?;
    cfg.slot(label.slot_id)
        .filter(|slot| slot.role == label.role)
        .map(|slot| slot.id)
}
pub fn scope(cfg: &Config) -> Vec<String> {
    let mut labels: Vec<_> = cfg
        .slots
        .iter()
        .map(|s| format!("praxis-{}-s{}-<token8>", s.role, s.id))
        .collect();
    labels.sort();
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_match_exact_slot_role_not_just_a_prefix_or_online_state() {
        let t = crate::test_support::TestApp::new();
        assert_eq!(
            matching_slot("praxis-llm-s1-deadbeef", &t.app.cfg()),
            Some(1)
        );
        for label in [
            "praxis-llm-s10-deadbeef",
            "praxis-media-s1-deadbeef",
            "other-llm-s1-deadbeef",
            "praxis-llm-s01-deadbeef",
            "praxis-llm-s1-deadbeef-more",
            "praxis-llm-s1-nonhex!!",
        ] {
            assert_eq!(matching_slot(label, &t.app.cfg()), None);
        }
    }
}
