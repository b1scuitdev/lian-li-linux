use super::*;
use lianli_devices::traits::RgbFrameDelivery;
use lianli_shared::rgb::{
    MergeLightingConfig, RgbDeviceConfig, RgbRenderFamily, RgbRenderProfile, RgbZoneConfig,
};
use std::{sync::mpsc, time::Duration};

struct Screen(mpsc::Sender<Vec<[u8; 3]>>);

#[derive(Default)]
struct CountedFan {
    count: std::sync::atomic::AtomicU16,
    applied: parking_lot::Mutex<Vec<(u16, [u8; 3])>>,
}

impl RgbDevice for CountedFan {
    fn device_name(&self) -> String {
        "Galahad".into()
    }
    fn supported_modes(&self) -> Vec<RgbMode> {
        vec![RgbMode::Static]
    }
    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        vec![RgbZoneInfo {
            name: "Fans".into(),
            led_count: self.count.load(std::sync::atomic::Ordering::Relaxed),
        }]
    }
    fn fan_led_count_control(&self) -> Option<lianli_shared::rgb::RgbLedCountControl> {
        Some(lianli_shared::rgb::RgbLedCountControl {
            zone: 0,
            min: 8,
            max: 50,
            default: 24,
        })
    }
    fn configure_fan_led_count(&self, count: Option<u16>) -> anyhow::Result<bool> {
        let count = self
            .fan_led_count_control()
            .unwrap()
            .resolve(count)
            .map_err(anyhow::Error::msg)?;
        Ok(self.count.swap(count, std::sync::atomic::Ordering::Relaxed) != count)
    }
    fn set_zone_effect(&self, _: u8, effect: &RgbEffect) -> anyhow::Result<()> {
        self.applied.lock().push((
            self.count.load(std::sync::atomic::Ordering::Relaxed),
            effect.colors[0],
        ));
        Ok(())
    }
    fn supports_mb_rgb_sync(&self) -> bool {
        true
    }
    fn set_mb_rgb_sync(&self, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn fan_led_count_only_save_reapplies_quick_sync_without_changing_membership_or_effect() {
    let (mut controller, mut config, _) = setup();
    let device = Arc::new(CountedFan::default());
    controller.replace_wired(HashMap::from([(
        "screen".into(),
        device.clone() as Arc<dyn RgbDevice>,
    )]));
    config.merge_lighting.as_mut().unwrap().kind = lianli_shared::rgb::RgbSyncKind::Matched;
    controller.validate_config(&config).unwrap();
    controller.apply_config(&config, &[]);
    assert_eq!(device.applied.lock().last(), Some(&(24, [0, 255, 0])));
    device.applied.lock().clear();
    controller.apply_config(&config, &[]);
    assert!(device.applied.lock().is_empty());
    config.devices[0].fan_led_count = Some(50);
    controller.apply_config(&config, &[]);
    assert_eq!(device.applied.lock().as_slice(), &[(50, [0, 255, 0])]);
    assert!(controller.sync_active.contains("screen"));
    let thermal = crate::thermal_alert::new_shared();
    controller.set_thermal_override(thermal.clone());
    *thermal.lock() = Some([255, 128, 0]);
    assert!(controller.check_thermal_override());
    assert_eq!(device.applied.lock().last(), Some(&(50, [255, 128, 0])));
    config.devices[0].fan_led_count = Some(8);
    controller.apply_config(&config, &[]);
    *thermal.lock() = None;
    assert!(!controller.check_thermal_override());
    assert_eq!(device.applied.lock().last(), Some(&(8, [0, 255, 0])));
    config.devices[0].fan_led_count = Some(50);
    controller.apply_config(&config, &[]);
    config.merge_lighting.as_mut().unwrap().enabled = false;
    controller.apply_config(&config, &[]);
    assert_eq!(device.applied.lock().last(), Some(&(50, [255, 0, 0])));
    config.devices[0].mb_rgb_sync = true;
    controller.apply_config(&config, &[]);
    config.devices[0].mb_rgb_sync = false;
    controller.apply_config(&config, &[]);
    assert_eq!(device.applied.lock().last(), Some(&(50, [255, 0, 0])));
    controller.invalidate_hardware_state();
    controller.apply_config(&config, &[]);
    assert_eq!(device.applied.lock().last(), Some(&(50, [255, 0, 0])));
    let reconnected = Arc::new(CountedFan::default());
    controller.replace_wired(HashMap::from([(
        "screen".into(),
        reconnected.clone() as Arc<dyn RgbDevice>,
    )]));
    controller.apply_config(&config, &[]);
    assert_eq!(reconnected.applied.lock().last(), Some(&(50, [255, 0, 0])));
    config.devices[0].fan_led_count = Some(51);
    assert!(controller.validate_config(&config).is_err());
    controller.replace_wired(HashMap::new());
    assert!(controller.validate_config(&config).is_ok());
}

impl RgbDevice for Screen {
    fn device_name(&self) -> String {
        "screen".into()
    }
    fn supported_modes(&self) -> Vec<RgbMode> {
        vec![RgbMode::Static]
    }
    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        vec![RgbZoneInfo {
            name: "ring".into(),
            led_count: 60,
        }]
    }
    fn set_zone_effect(&self, _: u8, _: &RgbEffect) -> anyhow::Result<()> {
        Ok(())
    }
    fn software_render_profile(&self) -> Option<RgbRenderProfile> {
        Some(RgbRenderProfile {
            family: RgbRenderFamily::UniversalScreen,
            fan_count: 0,
            led_count: 60,
            right_attach: false,
        })
    }
    fn software_frame_delivery(&self) -> Option<RgbFrameDelivery> {
        Some(RgbFrameDelivery::Streaming)
    }
    fn set_software_frames(&self, frames: &[Vec<[u8; 3]>], _: u16) -> anyhow::Result<()> {
        self.0.send(frames[0].clone())?;
        Ok(())
    }
}

fn setup() -> (RgbController, RgbAppConfig, mpsc::Receiver<Vec<[u8; 3]>>) {
    let (sender, received) = mpsc::channel();
    let device: Arc<dyn RgbDevice> = Arc::new(Screen(sender));
    let controller = RgbController::new(HashMap::from([("screen".into(), device)]), None);
    let config = RgbAppConfig {
        enabled: true,
        devices: vec![RgbDeviceConfig {
            device_id: "screen".into(),
            fan_led_count: None,
            mb_rgb_sync: false,
            active_preset: None,
            regions: None,
            effect_memory: Vec::new(),
            zones: vec![RgbZoneConfig {
                zone_index: 0,
                swap_lr: false,
                swap_tb: false,
                effect: RgbEffect {
                    colors: vec![[255, 0, 0]],
                    ..Default::default()
                },
            }],
        }],
        merge_lighting: Some(MergeLightingConfig {
            enabled: true,
            device_order: vec!["screen".into()],
            effect: RgbEffect {
                colors: vec![[0, 255, 0]],
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    };
    (controller, config, received)
}

struct BlockingSync {
    frames: mpsc::Sender<Vec<[u8; 3]>>,
    entered: mpsc::Sender<()>,
    release: parking_lot::Mutex<mpsc::Receiver<()>>,
    block: std::sync::atomic::AtomicBool,
}

impl RgbDevice for BlockingSync {
    fn device_name(&self) -> String {
        "Sync output".into()
    }
    fn supported_modes(&self) -> Vec<RgbMode> {
        vec![RgbMode::Static]
    }
    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        vec![RgbZoneInfo {
            name: "Lighting".into(),
            led_count: 9,
        }]
    }
    fn software_render_profile(&self) -> Option<RgbRenderProfile> {
        Some(RgbRenderProfile {
            family: RgbRenderFamily::P28,
            fan_count: 1,
            led_count: 9,
            right_attach: false,
        })
    }
    fn software_frame_delivery(&self) -> Option<RgbFrameDelivery> {
        Some(RgbFrameDelivery::Streaming)
    }
    fn set_zone_effect(&self, _: u8, _: &RgbEffect) -> anyhow::Result<()> {
        anyhow::bail!("sync output required")
    }
    fn set_software_frames(&self, _: &[Vec<[u8; 3]>], _: u16) -> anyhow::Result<()> {
        anyhow::bail!("sync output required")
    }
    fn set_sync_animation(
        &self,
        frames: &[Vec<[u8; 3]>],
        _: lianli_shared::rgb::RgbPlaybackTiming,
    ) -> anyhow::Result<()> {
        if frames[0].iter().any(|color| *color != [0; 3])
            && self.block.swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            self.entered.send(())?;
            self.release.lock().recv_timeout(Duration::from_secs(2))?;
        }
        self.frames.send(frames[0].clone())?;
        Ok(())
    }
}

#[test]
fn continuous_merge_black_follows_inflight_output_and_resumes_current_config() {
    let (frames, received) = mpsc::channel();
    let (entered, waiting) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let (mut controller, mut config, other) = setup();
    let screen = controller.clone_wired_device("screen").unwrap();
    controller.replace_wired(HashMap::from([
        ("screen".into(), screen),
        (
            "sync".into(),
            Arc::new(BlockingSync {
                frames,
                entered,
                release: parking_lot::Mutex::new(blocked),
                block: std::sync::atomic::AtomicBool::new(true),
            }) as Arc<dyn RgbDevice>,
        ),
    ]));
    config.devices.clear();
    config.devices.push(RgbDeviceConfig {
        device_id: "offline".into(),
        fan_led_count: None,
        mb_rgb_sync: false,
        active_preset: None,
        regions: None,
        effect_memory: Vec::new(),
        zones: vec![RgbZoneConfig {
            zone_index: 0,
            effect: RgbEffect::default(),
            swap_lr: false,
            swap_tb: false,
        }],
    });
    let sync = config.merge_lighting.as_mut().unwrap();
    sync.kind = lianli_shared::rgb::RgbSyncKind::Continuous;
    sync.device_order = vec!["screen".into(), "offline".into(), "sync".into()];
    sync.effect.mode = RgbMode::Rainbow;
    controller.validate_config(&config).unwrap();
    controller.apply_config(&config, &[]);
    waiting.recv_timeout(Duration::from_secs(1)).unwrap();
    controller.set_night_mode(true).unwrap();
    assert!(controller.native_night_mode_engaged());
    release.send(()).unwrap();
    assert!(received
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .iter()
        .any(|color| *color != [0; 3]));
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0; 3]; 9]
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "sync output never became black"
        );
        if other
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .iter()
            .all(|color| *color == [0; 3])
        {
            break;
        }
    }
    assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
    assert!(other.try_recv().is_err());
    let original = config.clone();
    let sync = config.merge_lighting.as_mut().unwrap();
    sync.effect.mode = RgbMode::Static;
    sync.effect.colors = vec![[0, 0, 255]];
    sync.device_order.reverse();
    controller.validate_config(&config).unwrap();
    controller.apply_config(&config, &[]);
    controller.set_night_mode(true).unwrap();
    assert_eq!(controller.config.as_ref(), Some(&config));
    assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
    assert!(other.try_recv().is_err());
    let mut removed = config.clone();
    removed
        .merge_lighting
        .as_mut()
        .unwrap()
        .device_order
        .retain(|id| id != "sync");
    assert!(controller
        .validate_config(&removed)
        .unwrap_err()
        .to_string()
        .contains("Disable Night Mode"));
    controller.set_night_mode(false).unwrap();
    for frame in [
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        other.recv_timeout(Duration::from_secs(1)).unwrap(),
    ] {
        assert!(frame
            .iter()
            .all(|color| color[0] == 0 && color[1] == 0 && color[2] > 0));
    }
    assert_eq!(
        controller.sync_active,
        std::collections::HashSet::from(["screen".into(), "sync".into()])
    );
    assert!(original.merge_lighting.unwrap().enabled);
    assert!(config.merge_lighting.as_ref().unwrap().enabled);
    controller.set_night_mode(false).unwrap();
    assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
    assert!(other.try_recv().is_err());
}

#[test]
fn missing_cached_wireless_sync_target_does_not_prevent_healthy_blackout() {
    let (mut controller, config, received) = setup();
    controller.wireless = Some(Arc::new(WirelessController::new()));
    controller.apply_config(&config, &[]);
    received.recv_timeout(Duration::from_secs(1)).unwrap();
    controller.wireless_state.insert(
        "wireless".into(),
        WirelessDevice {
            mac: [1; 6],
            fan_type: WirelessFanType::SlV4,
            fan_count: 1,
            right_attach: false,
        },
    );
    controller.sync_active.insert("wireless".into());
    controller
        .config
        .as_mut()
        .unwrap()
        .merge_lighting
        .as_mut()
        .unwrap()
        .device_order
        .push("wireless".into());
    let error = controller.set_night_mode(true).unwrap_err().to_string();
    assert!(error.contains("wireless"));
    assert!(controller.native_night_mode_engaged());
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "healthy sync output never became black"
        );
        let frame = received.recv_timeout(Duration::from_secs(1)).unwrap();
        if frame.iter().all(|color| *color == [0; 3]) {
            break;
        }
    }
    assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
    controller.wireless_state.clear();
    controller.set_night_mode(true).unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0; 3]; 60]
    );
    controller.set_night_mode(false).unwrap();
    assert!(received
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .iter()
        .all(|color| color[1] > 0));
}

#[test]
fn matched_hardware_merge_blackout_restores_current_sync_effect() {
    let (mut controller, mut config, _) = setup();
    let device = Arc::new(CountedFan::default());
    device.count.store(24, std::sync::atomic::Ordering::Relaxed);
    controller.replace_wired(HashMap::from([(
        "screen".into(),
        device.clone() as Arc<dyn RgbDevice>,
    )]));
    config.merge_lighting.as_mut().unwrap().kind = lianli_shared::rgb::RgbSyncKind::Matched;
    controller.apply_config(&config, &[]);
    device.applied.lock().clear();
    controller.set_night_mode(true).unwrap();
    assert!(controller.native_night_mode_engaged());
    assert_eq!(device.applied.lock().as_slice(), [(24, [0; 3])]);
    config.merge_lighting.as_mut().unwrap().effect.colors = vec![[0, 0, 255]];
    controller.validate_config(&config).unwrap();
    controller.apply_config(&config, &[]);
    assert_eq!(device.applied.lock().as_slice(), [(24, [0; 3])]);
    controller.set_night_mode(false).unwrap();
    assert_eq!(
        device.applied.lock().as_slice(),
        [(24, [0; 3]), (24, [0, 0, 255])]
    );
}

#[test]
fn night_mode_restores_updated_merge_led_count_after_thermal_alert_clears() {
    let (mut controller, mut config, _) = setup();
    let device = Arc::new(CountedFan::default());
    controller.replace_wired(HashMap::from([(
        "screen".into(),
        device.clone() as Arc<dyn RgbDevice>,
    )]));
    config.merge_lighting.as_mut().unwrap().kind = lianli_shared::rgb::RgbSyncKind::Matched;
    controller.apply_config(&config, &[]);
    controller.set_night_mode(true).unwrap();
    config.devices[0].fan_led_count = Some(50);
    controller.validate_config(&config).unwrap();
    controller.apply_config(&config, &[]);
    assert_eq!(device.applied.lock().last(), Some(&(24, [0; 3])));
    *controller.thermal_override.lock() = Some([255, 128, 0]);
    controller.set_night_mode(false).unwrap();
    assert_eq!(device.applied.lock().last(), Some(&(24, [255, 128, 0])));
    *controller.thermal_override.lock() = None;
    assert!(!controller.check_thermal_override());
    assert_eq!(device.applied.lock().last(), Some(&(50, [0, 255, 0])));
}

#[test]
fn night_mode_keeps_sync_config_changes_dark_until_release() {
    let (mut controller, mut config, received) = setup();
    controller.apply_config(&config, &[]);
    received.recv_timeout(Duration::from_secs(1)).unwrap();
    controller.set_night_mode(true).unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0; 3]; 60]
    );
    config.merge_lighting.as_mut().unwrap().effect.colors = vec![[0, 0, 255]];
    controller.validate_config(&config).unwrap();
    controller.apply_config(&config, &[]);
    controller.set_night_mode(true).unwrap();
    assert!(received.recv_timeout(Duration::from_millis(80)).is_err());
    controller.set_night_mode(false).unwrap();
    let frame = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(frame
        .iter()
        .all(|color| color[0] == 0 && color[1] == 0 && color[2] > 0));
    assert!(controller.sync_active.contains("screen"));
}

#[test]
fn night_mode_releases_the_latest_software_preset_colors() {
    let (mut controller, mut config, received) = setup();
    config.merge_lighting = None;
    config.devices[0].active_preset = Some("Palette".into());
    let mut presets = vec![RgbPreset {
        name: "Palette".into(),
        device_id: "screen".into(),
        zones: vec![RgbPresetZone {
            zone: 0,
            colors: vec![[255, 0, 0]; 60],
            effect: None,
        }],
        regions: None,
    }];
    controller.apply_config(&config, &presets);
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[255, 0, 0]; 60]
    );
    controller.set_night_mode(true).unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0; 3]; 60]
    );
    presets[0].zones[0].colors = vec![[0, 0, 255]; 60];
    controller.apply_config(&config, &presets);
    assert!(received.recv_timeout(Duration::from_millis(80)).is_err());
    controller.set_night_mode(false).unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0, 0, 255]; 60]
    );
}

#[test]
fn sync_deduplicates_configuration_and_restores_individual_settings() {
    let (mut controller, mut config, received) = setup();
    controller.validate_config(&config).unwrap();
    controller.apply_config(&config, &[]);
    let frame = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(frame.iter().all(|rgb| rgb[0] == 0 && rgb[1] > 0));
    assert!(controller
        .set_effect("screen", 0, &RgbEffect::default())
        .is_err());

    config
        .merge_lighting
        .as_mut()
        .unwrap()
        .effect_memory
        .push(RgbEffect::default());
    controller.apply_config(&config, &[]);
    assert!(received.recv_timeout(Duration::from_millis(80)).is_err());

    config.merge_lighting.as_mut().unwrap().enabled = false;
    controller.apply_config(&config, &[]);
    let frame = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(frame.iter().all(|rgb| rgb[0] > 0 && rgb[1] == 0));
    assert!(controller.sync_active.is_empty());
    controller.stop();
}

#[test]
fn failed_sync_preflight_preserves_current_playback() {
    let (mut controller, mut config, received) = setup();
    controller.apply_config(&config, &[]);
    received.recv_timeout(Duration::from_secs(1)).unwrap();
    let signature = controller.sync_signature.clone();
    config.merge_lighting.as_mut().unwrap().effect.mode = RgbMode::Voice;
    assert!(controller.validate_config(&config).is_err());
    controller.apply_config(&config, &[]);
    assert_eq!(controller.sync_signature, signature);
    assert!(controller.sync_active.contains("screen"));
    assert!(received.recv_timeout(Duration::from_millis(80)).is_err());
    controller.stop();
}

#[test]
fn openrgb_release_restores_sync_without_individual_animation_upload() {
    let (mut controller, config, received) = setup();
    controller.apply_config(&config, &[]);
    received.recv_timeout(Duration::from_secs(1)).unwrap();
    controller.set_openrgb_active(true).unwrap();
    assert!(controller.sync_active.is_empty());
    controller.set_openrgb_active(false).unwrap();
    let frame = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(frame.iter().all(|rgb| rgb[0] == 0 && rgb[1] > 0));
    assert!(received.recv_timeout(Duration::from_millis(80)).is_err());
    controller.stop();
}

struct UnavailableDevice;

struct SharedPort {
    port: usize,
    colors: Arc<parking_lot::Mutex<[[u8; 3]; 2]>>,
}

impl RgbDevice for SharedPort {
    fn device_name(&self) -> String {
        format!("port {}", self.port)
    }
    fn supported_modes(&self) -> Vec<RgbMode> {
        vec![RgbMode::Static]
    }
    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        vec![RgbZoneInfo {
            name: "fan".into(),
            led_count: 1,
        }]
    }
    fn set_zone_effect(&self, _: u8, effect: &RgbEffect) -> anyhow::Result<()> {
        self.colors.lock()[self.port] = effect.colors[0];
        Ok(())
    }
    fn supports_mb_rgb_sync(&self) -> bool {
        true
    }
    fn set_mb_rgb_sync(&self, _: bool) -> anyhow::Result<()> {
        self.colors.lock().fill([0; 3]);
        Ok(())
    }
}

#[test]
fn sync_resets_shared_controller_before_applying_either_port() {
    for order in [
        vec!["port0".into(), "port1".into()],
        vec!["port1".into(), "port0".into()],
    ] {
        let colors = Arc::new(parking_lot::Mutex::new([[0; 3]; 2]));
        let ports = (0..2)
            .map(|port| {
                (
                    format!("port{port}"),
                    Arc::new(SharedPort {
                        port,
                        colors: colors.clone(),
                    }) as Arc<dyn RgbDevice>,
                )
            })
            .collect();
        let mut controller = RgbController::new(ports, None);
        let config = RgbAppConfig {
            enabled: true,
            merge_lighting: Some(MergeLightingConfig {
                enabled: true,
                kind: lianli_shared::rgb::RgbSyncKind::Matched,
                device_order: order,
                effect: RgbEffect {
                    colors: vec![[17, 38, 59]],
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        controller.apply_config(&config, &[]);
        assert_eq!(*colors.lock(), [[17, 38, 59]; 2]);
        controller.stop();
    }
}

#[test]
fn shared_controller_reset_restores_unchanged_individual_port() {
    let colors = Arc::new(parking_lot::Mutex::new([[0; 3]; 2]));
    let ports = (0..2)
        .map(|port| {
            (
                format!("hid:controller:port{port}"),
                Arc::new(SharedPort {
                    port,
                    colors: colors.clone(),
                }) as Arc<dyn RgbDevice>,
            )
        })
        .collect();
    let mut controller = RgbController::new(ports, None);
    let device: RgbDeviceConfig = serde_json::from_value(serde_json::json!({
        "device_id": "hid:controller:port1",
        "zones": [{"zone_index": 0, "effect": RgbEffect { colors: vec![[30, 40, 50]], ..Default::default() }}]
    })).unwrap();
    let mut config = RgbAppConfig {
        enabled: true,
        devices: vec![device],
        ..Default::default()
    };
    controller.apply_config(&config, &[]);
    assert_eq!(colors.lock()[1], [30, 40, 50]);
    config.merge_lighting = Some(MergeLightingConfig {
        enabled: true,
        kind: lianli_shared::rgb::RgbSyncKind::Matched,
        device_order: vec!["hid:controller:port0".into()],
        effect: RgbEffect {
            colors: vec![[1, 2, 3]],
            ..Default::default()
        },
        ..Default::default()
    });
    controller.apply_config(&config, &[]);
    assert_eq!(*colors.lock(), [[1, 2, 3], [30, 40, 50]]);
    controller.stop();
}

impl RgbDevice for UnavailableDevice {
    fn device_name(&self) -> String {
        "unavailable".into()
    }
    fn supported_modes(&self) -> Vec<RgbMode> {
        vec![RgbMode::Static]
    }
    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        vec![RgbZoneInfo {
            name: "zone".into(),
            led_count: 1,
        }]
    }
    fn set_zone_effect(&self, _: u8, _: &RgbEffect) -> anyhow::Result<()> {
        anyhow::bail!("device disappeared")
    }
}

#[test]
fn one_failed_sync_device_does_not_block_other_participants_or_restoration() {
    let (mut controller, mut config, restored) = setup();
    controller.apply_config(&config, &[]);
    restored.recv_timeout(Duration::from_secs(1)).unwrap();
    let (sender, participating) = mpsc::channel();
    controller
        .wired
        .insert("unavailable".into(), Arc::new(UnavailableDevice));
    controller
        .wired
        .insert("participant".into(), Arc::new(Screen(sender)));
    let sync = config.merge_lighting.as_mut().unwrap();
    sync.kind = lianli_shared::rgb::RgbSyncKind::Matched;
    sync.device_order = vec!["unavailable".into(), "participant".into()];
    controller.apply_config(&config, &[]);
    assert!(participating
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .iter()
        .all(|rgb| rgb[0] == 0 && rgb[1] > 0));
    assert!(restored
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .iter()
        .all(|rgb| rgb[0] > 0 && rgb[1] == 0));
    assert!(controller.sync_active.contains("unavailable"));
    assert!(controller.sync_signature.is_none());
    controller.stop();
}
