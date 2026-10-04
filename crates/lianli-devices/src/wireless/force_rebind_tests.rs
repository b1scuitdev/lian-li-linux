use super::super::discovery::RawDiagnostic;
use super::tests::{controller_with, seed_device};
use super::*;

const TARGET: [u8; 6] = [0x8b, 0x92, 0xcc, 0x88, 0x19, 0x70];
const MASTER: [u8; 6] = [9; 6];

fn diagnostic_controller() -> WirelessController {
    let controller = controller_with(MASTER, false);
    let mut raw = [0; 42];
    raw[..6].copy_from_slice(&TARGET);
    raw[6..12].copy_from_slice(&MASTER);
    raw[13] = 55;
    raw[19] = 3;
    raw[24..27].copy_from_slice(&[23; 3]);
    raw[36..40].copy_from_slice(&[110, 120, 130, 0]);
    raw[41] = 0x1c;
    *controller.receiver_state.diagnostic.lock() = Some(RawDiagnostic {
        target: TARGET,
        latest: Some((raw, Instant::now())),
        sightings: 3,
        occupied: [false; RX_SLOT_LIMIT as usize],
        attempted: false,
    });
    controller
}

#[test]
fn malformed_target_uses_vendor_payload_and_broadcast_route_without_publication() {
    let controller = diagnostic_controller();
    seed_device(&controller, &[1; 6], MASTER, true);
    let healthy = controller.device_health.lock()[&[1; 6]].published.clone();
    controller.discovered_devices.lock().push(healthy.clone());
    let plan = controller.diagnostic_bind_plan(&TARGET).unwrap();
    assert_eq!(plan.outer_rx, 0xff);
    assert_eq!(plan.channel, 8);
    assert_eq!(plan.rx, 2);
    assert_eq!(plan.slot, 2);
    assert_eq!(
        &plan.data[..21],
        &[
            0x12, 0x10, 0x8b, 0x92, 0xcc, 0x88, 0x19, 0x70, 9, 9, 9, 9, 9, 9, 2, 8, 2, 110, 120,
            130, 0,
        ]
    );
    assert_eq!(plan.data.len(), 240);
    assert!(plan.data[21..].iter().all(|byte| *byte == 0));
    assert!(!controller.device_health.lock().contains_key(&TARGET));
    let devices = controller.devices();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].mac, healthy.mac);
    assert_eq!(devices[0].rx_type, healthy.rx_type);
    assert_eq!(devices[0].current_pwm, healthy.current_pwm);
}

#[test]
fn allocation_reserves_raw_published_and_all_observed_slots() {
    let controller = diagnostic_controller();
    seed_device(&controller, &[1; 6], MASTER, true);
    {
        let mut health = controller.device_health.lock();
        let healthy = health.get_mut(&[1; 6]).unwrap();
        healthy.raw_rx = 2;
        healthy.published.rx_type = 1;
    }
    {
        let mut capture = controller.receiver_state.diagnostic.lock();
        let capture = capture.as_mut().unwrap();
        capture.occupied[3] = true;
    }
    let plan = controller.diagnostic_bind_plan(&TARGET).unwrap();
    assert_eq!(plan.rx, 4);
    assert_eq!(plan.data[14], 4);
    assert_eq!(plan.data[16], 1);
    controller
        .receiver_state
        .diagnostic
        .lock()
        .as_mut()
        .unwrap()
        .occupied
        .fill(true);
    assert!(controller.diagnostic_bind_plan(&TARGET).is_err());
}

#[test]
fn invalid_master_stale_missing_or_wrong_target_and_repeat_attempt_abort() {
    let controller = diagnostic_controller();
    controller
        .rx_running
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(controller.diagnostic_bind_plan(&TARGET).is_err());
    controller
        .rx_running
        .store(false, std::sync::atomic::Ordering::Release);
    *controller.master_mac.lock() = [0; 6];
    assert!(controller.diagnostic_bind_plan(&TARGET).is_err());
    *controller.master_mac.lock() = MASTER;
    for channel in [0, 40, 255] {
        *controller.master_channel.lock() = channel;
        assert!(controller.diagnostic_bind_plan(&TARGET).is_err());
    }
    *controller.master_channel.lock() = 8;
    assert!(controller.diagnostic_bind_plan(&[1; 6]).is_err());
    {
        let mut capture = controller.receiver_state.diagnostic.lock();
        let capture = capture.as_mut().unwrap();
        capture.attempted = true;
    }
    assert!(controller.diagnostic_bind_plan(&TARGET).is_err());
    {
        let mut capture = controller.receiver_state.diagnostic.lock();
        let capture = capture.as_mut().unwrap();
        capture.attempted = false;
        capture.latest.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(4);
    }
    assert!(controller.diagnostic_bind_plan(&TARGET).is_err());
    *controller.receiver_state.diagnostic.lock() = None;
    assert!(controller.diagnostic_bind_plan(&TARGET).is_err());
}

#[test]
fn preflight_refuses_normal_record_foreign_master_bad_marker_or_wrong_family() {
    for (offset, value) in [(12, 8), (6, 7), (41, 0), (18, 10), (19, 0), (19, 5)] {
        let controller = diagnostic_controller();
        controller
            .receiver_state
            .diagnostic
            .lock()
            .as_mut()
            .unwrap()
            .latest
            .as_mut()
            .unwrap()
            .0[offset] = value;
        assert!(controller.diagnostic_bind_plan(&TARGET).is_err());
    }
    let controller = diagnostic_controller();
    controller
        .receiver_state
        .diagnostic
        .lock()
        .as_mut()
        .unwrap()
        .latest
        .as_mut()
        .unwrap()
        .0[24..28]
        .fill(20);
    assert!(controller.diagnostic_bind_plan(&TARGET).is_err());
}
