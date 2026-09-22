use super::*;

#[test]
fn capacity_wait_never_times_out_replaces_or_defends_a_stale_bid() {
    for phase in [InstanceState::Requested,InstanceState::StartRequested,InstanceState::StartFailed,InstanceState::Scheduling,InstanceState::Provisioning] {
        for (id,role) in [(1,Role::Llm),(2,Role::Media)] {
            let mut i=inst(11,id,role,phase);
            i.boot_started_at=now()-chrono::Duration::days(2);
            i.actual_status="running".into(); // deliberately conflicting old provider observation
            i.min_bid=9.0;
            let mut s=slot(id,role,vec![i],Some(11));s.desired_running=true;
            let actions=decide(&snap(vec![s]),&cfg());
            assert!(!actions.iter().any(|a|!matches!(a,Action::Alert{..})),"{phase:?}: {actions:?}");
        }
    }
}

#[test]
fn explicit_hard_budget_still_cancels_queued_capacity_but_slot_lock_protects_every_backer() {
    for hard_action in ["stop","destroy"] {
        let mut c=cfg();c.budget.hard_action=hard_action.into();
        let mut a=inst(11,1,Role::Llm,InstanceState::Scheduling);a.actual_status="scheduling".into();
        let b=inst(12,1,Role::Llm,InstanceState::Healthy);
        let mut s=snap(vec![slot(1,Role::Llm,vec![a,b],Some(12))]);s.spent_today_usd=99.0;
        assert!(decide(&s,&c).iter().any(|a|match a {Action::Stop{instance_id,..}|Action::Destroy{instance_id,..}=>*instance_id==11,_=>false}));
        s.slots[0].pinned=true;
        assert!(decide(&s,&c).iter().all(|a|matches!(a,Action::Alert{..})));
    }
}

#[test]
fn explicit_lifecycle_deadline_cancels_queue_without_wait_timeout_or_disk_deletion() {
    let mut a=inst(11,1,Role::Llm,InstanceState::Scheduling);a.actual_status="scheduling".into();
    a.lifecycle=praxis_common::Lifecycle::Ttl{ttl_s:1,destroy:false};
    let actions=decide(&snap(vec![slot(1,Role::Llm,vec![a],Some(11))]),&cfg());
    assert!(actions.iter().any(|a|matches!(a,Action::Stop{instance_id:11,..})));
    assert!(!actions.iter().any(|a|matches!(a,Action::Destroy{..}|Action::Create{..})));
}

#[test]
fn ended_lifecycle_window_cancels_pending_start() {
    let mut a=inst(11,1,Role::Llm,InstanceState::StartRequested);a.actual_status="stopped".into();
    a.lifecycle=praxis_common::Lifecycle::Schedule{spec:"Mo-Fr 09:00-10:00".into(),prewarm_s:0,destroy:false};
    let mut s=slot(1,Role::Llm,vec![a],Some(11));s.local_weekday=1;s.local_minutes_of_day=12*60;
    assert!(decide(&snap(vec![s]),&cfg()).iter().any(|a|matches!(a,Action::Stop{instance_id:11,..})));
}

#[test]
fn ready_alternative_can_take_over_but_a_queued_media_disk_is_retained() {
    let a=inst(11,2,Role::Media,InstanceState::Scheduling);
    let b=inst(12,2,Role::Media,InstanceState::Healthy);
    let actions=decide(&snap(vec![slot(2,Role::Media,vec![a,b],Some(11))]),&cfg());
    assert!(actions.iter().any(|a|matches!(a,Action::FlipSlot{to_instance:12,..})));
    assert!(actions.iter().any(|a|matches!(a,Action::SwapOut{instance_id:11,destroy:false,..})));
    assert!(!actions.iter().any(|a|matches!(a,Action::Destroy{instance_id:11,..}|Action::SwapOut{instance_id:11,destroy:true,..})));
}
