use super::*;
fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} != {b}");
}

#[test]
fn provider_total_storage_and_compute_are_normalized_exactly_once() {
    let mut instance:Instance=serde_json::from_value(serde_json::json!({"dph_base":0.4573333333,"dph_total":0.4684444444,"storage_total_cost":0.0111111111})).unwrap();
    close(instance.on_demand_compute_usd_h(0.5).unwrap(), 0.4573333333);
    instance.dph_base = None;
    close(instance.on_demand_compute_usd_h(0.5).unwrap(), 0.4573333333);
    instance.storage_total_cost = None;
    close(
        instance.on_demand_compute_usd_h(0.0111111111).unwrap(),
        0.4573333333,
    );
    let offer: Offer =
        serde_json::from_value(serde_json::json!({"dph_total":0.21,"storage_cost":0.12})).unwrap();
    close(offer.on_demand_compute_usd_h(60).unwrap(), 0.20);
    close(offer.storage_usd_h(60), 0.01);
    close(offer.quoted_storage_cost(60).unwrap(), 0.12);
    let allocated = Offer {
        storage_total_cost: Some(0.01),
        ..Default::default()
    };
    close(allocated.quoted_storage_cost(60).unwrap(), 0.12);
}

#[test]
fn absent_nonfinite_and_nonpositive_compute_quotes_fail_closed() {
    assert_eq!(Offer::default().on_demand_compute_usd_h(120), None);
    assert_eq!(Offer::default().quoted_storage_cost(120), None);
    assert_eq!(
        Offer {
            storage_cost: Some(0.0),
            ..Default::default()
        }
        .quoted_storage_cost(120),
        Some(0.0)
    );
    for base in [0.0, -1.0, f64::INFINITY, f64::NAN] {
        let offer = Offer {
            dph_base: Some(base),
            dph_total: Some(1.0),
            ..Default::default()
        };
        assert_eq!(offer.on_demand_compute_usd_h(120), None);
    }
    for storage in [-1.0, f64::INFINITY, f64::NAN, 2.0] {
        let offer = Offer {
            dph_total: Some(1.0),
            storage_total_cost: Some(storage),
            ..Default::default()
        };
        assert_eq!(offer.on_demand_compute_usd_h(120), None);
    }
}
