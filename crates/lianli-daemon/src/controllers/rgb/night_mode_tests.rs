use super::*;
use lianli_devices::traits::RgbFrameDelivery;
use lianli_shared::rgb::{RgbDeviceConfig, RgbZoneConfig};
use parking_lot::Mutex;
use std::sync::mpsc;
use std::time::Duration;

struct RecordingRgb {
    effects: mpsc::Sender<RgbEffect>,
    sync_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl RgbDevice for RecordingRgb {
    fn device_name(&self) -> String {
        "Test RGB".into()
    }
    fn supported_modes(&self) -> Vec<RgbMode> {
        vec![RgbMode::Off, RgbMode::Static, RgbMode::Direct]
    }
    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        vec![RgbZoneInfo {
            name: "Lighting".into(),
            led_count: 2,
        }]
    }
    fn set_zone_effect(&self, _: u8, effect: &RgbEffect) -> anyhow::Result<()> {
        self.effects.send(effect.clone())?;
        Ok(())
    }
    fn supports_mb_rgb_sync(&self) -> bool {
        true
    }
    fn set_mb_rgb_sync(&self, _: bool) -> anyhow::Result<()> {
        self.sync_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn supports_direction(&self) -> bool {
        true
    }
    fn set_fan_direction(&self, _: u8, _: bool, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn ping(&self, _: u8) -> anyhow::Result<()> {
        Ok(())
    }
}

fn config(openrgb_server: bool) -> RgbAppConfig {
    RgbAppConfig {
        openrgb_server,
        devices: vec![RgbDeviceConfig {
            device_id: "device".into(),
            fan_led_count: None,
            mb_rgb_sync: false,
            active_preset: None,
            regions: None,
            effect_memory: Vec::new(),
            zones: vec![RgbZoneConfig {
                zone_index: 0,
                effect: RgbEffect {
                    colors: vec![[7, 8, 9]],
                    ..Default::default()
                },
                swap_lr: false,
                swap_tb: false,
            }],
        }],
        ..Default::default()
    }
}

fn controller() -> (RgbController, mpsc::Receiver<RgbEffect>) {
    let (rgb, received, _) = controller_with_sync_calls();
    (rgb, received)
}

fn controller_with_sync_calls() -> (
    RgbController,
    mpsc::Receiver<RgbEffect>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let (effects, received) = mpsc::channel();
    let sync_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    (
        RgbController::new(
            HashMap::from([(
                "device".into(),
                Arc::new(RecordingRgb {
                    effects,
                    sync_calls: sync_calls.clone(),
                }) as Arc<dyn RgbDevice>,
            )]),
            None,
        ),
        received,
        sync_calls,
    )
}

#[test]
fn unsafe_native_configuration_bypasses_night_mode_without_rgb_writes() {
    for variant in 0..6 {
        let (mut rgb, received) = controller();
        let mut saved = config(false);
        match variant {
            0 => saved.enabled = false,
            1 => saved.devices.clear(),
            2 => saved.devices[0].zones.clear(),
            3 => saved.devices[0].zones[0].effect.mode = RgbMode::Rainbow,
            4 => saved.devices[0].active_preset = Some("missing".into()),
            _ => saved.devices[0].regions = Some(Vec::new()),
        }
        rgb.apply_config(&saved, &[]);
        while received.try_recv().is_ok() {}
        rgb.set_night_mode(true).unwrap();
        assert!(rgb.night_mode_active);
        assert!(!rgb.output_override_active());
        assert!(received.try_recv().is_err());
        rgb.set_effect("device", 0, &RgbEffect::default()).unwrap();
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        *rgb.thermal_override.lock() = Some([255, 128, 0]);
        assert!(rgb.check_thermal_override());
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .colors,
            [[255, 128, 0]]
        );
        rgb.set_night_mode(false).unwrap();
        assert!(received.try_recv().is_err());
    }
}

#[test]
fn native_night_mode_preserves_effective_motherboard_sync() {
    for enabled in [false, true] {
        let (mut rgb, received, sync_calls) = controller_with_sync_calls();
        let mut saved = config(false);
        saved.devices[0].mb_rgb_sync = enabled;
        rgb.apply_config(&saved, &[]);
        while received.try_recv().is_ok() {}
        let before = sync_calls.load(Ordering::Relaxed);
        rgb.set_night_mode(true).unwrap();
        assert!(rgb.night_mode_active);
        assert_eq!(rgb.output_override_active(), !enabled);
        if !enabled {
            assert_eq!(
                received.recv_timeout(Duration::from_secs(1)).unwrap().mode,
                RgbMode::Off
            );
        }
        assert_eq!(sync_calls.load(Ordering::Relaxed), before);
        rgb.set_night_mode(false).unwrap();
        assert_eq!(sync_calls.load(Ordering::Relaxed), before);
        if !enabled {
            assert_eq!(
                received.recv_timeout(Duration::from_secs(1)).unwrap(),
                saved.devices[0].zones[0].effect
            );
        }
        assert!(received.try_recv().is_err());
    }
}

#[test]
fn destructive_config_changes_are_rejected_only_when_rgb_blackout_is_engaged() {
    for variant in 0..4 {
        let (mut rgb, received) = controller();
        let saved = config(false);
        rgb.apply_config(&saved, &[]);
        received.recv().unwrap();
        rgb.set_night_mode(true).unwrap();
        received.recv().unwrap();
        let mut changed = saved.clone();
        match variant {
            0 => changed.enabled = false,
            1 => changed.devices.clear(),
            2 => changed.devices[0].mb_rgb_sync = true,
            _ => changed.openrgb_server = true,
        }
        assert!(rgb
            .validate_config(&changed)
            .unwrap_err()
            .to_string()
            .contains("Disable Night Mode"));
        rgb.apply_config(&changed, &[]);
        assert!(rgb.night_mode_active);
        assert!(rgb.native_night_mode_engaged());
        assert_eq!(rgb.config.as_ref(), Some(&saved));
        assert!(received.try_recv().is_err());
        rgb.set_night_mode(false).unwrap();
        assert_eq!(received.recv().unwrap(), saved.devices[0].zones[0].effect);
        let mut bypassed = saved.clone();
        bypassed.openrgb_server = true;
        rgb.apply_config(&bypassed, &[]);
        rgb.set_night_mode(true).unwrap();
        assert!(!rgb.output_override_active());
        rgb.validate_config(&changed).unwrap();
        rgb.apply_config(&changed, &[]);
        assert_eq!(rgb.config, Some(changed));
        assert!(received.try_recv().is_err());
    }
}

#[test]
fn openrgb_ownership_changes_require_releasing_native_blackout() {
    let (mut rgb, received) = controller();
    let saved = config(false);
    rgb.apply_config(&saved, &[]);
    received.recv().unwrap();
    rgb.set_night_mode(true).unwrap();
    received.recv().unwrap();
    assert!(rgb
        .set_openrgb_active(true)
        .unwrap_err()
        .to_string()
        .contains("Disable Night Mode"));
    assert!(!rgb.is_openrgb_controlled());
    assert!(received.try_recv().is_err());
    rgb.set_night_mode(false).unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        saved.devices[0].zones[0].effect
    );
    rgb.set_openrgb_active(true).unwrap();
    rgb.set_night_mode(true).unwrap();
    assert!(rgb.night_mode_active);
    assert!(rgb.is_openrgb_controlled());
    assert!(!rgb.output_override_active());
    rgb.set_direct_colors("device", 0, &[[3; 3]; 2]).unwrap();
    assert_eq!(received.recv().unwrap().colors, [[3; 3]]);
    rgb.set_openrgb_active(false).unwrap();
    assert_eq!(received.recv().unwrap(), saved.devices[0].zones[0].effect);
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    assert!(rgb.native_night_mode_engaged());
    assert!(rgb.night_mode_active);
}

#[test]
fn failed_pre_transition_restore_keeps_the_old_source_and_can_be_retried() {
    for ownership_change in [false, true] {
        let (mut rgb, device, received) = fallible_controller();
        let saved = config(false);
        rgb.apply_config(&saved, &[]);
        received.recv().unwrap();
        rgb.set_night_mode(true).unwrap();
        received.recv().unwrap();
        let mut changed = saved.clone();
        changed.enabled = false;
        device.failed.store(true, Ordering::Relaxed);
        assert!(rgb.set_night_mode(false).is_err());
        for _ in 0..2 {
            if ownership_change {
                assert!(rgb.set_openrgb_active(true).is_err());
            } else {
                rgb.apply_config(&changed, &[]);
            }
            assert_eq!(rgb.config.as_ref(), Some(&saved));
            assert!(!rgb.is_openrgb_controlled());
            assert!(rgb
                .override_error
                .as_ref()
                .unwrap()
                .contains("device disconnected"));
            assert!(rgb.native_night_mode_restore_pending);
        }
        device.failed.store(false, Ordering::Relaxed);
        if ownership_change {
            rgb.set_openrgb_active(true).unwrap();
            assert!(rgb.is_openrgb_controlled());
        } else {
            rgb.apply_config(&changed, &[]);
            assert_eq!(rgb.config, Some(changed));
        }
        assert_eq!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            saved.devices[0].zones[0].effect
        );
        assert!(rgb.override_error.is_none());
        assert!(!rgb.native_night_mode_restore_pending);
        assert!(!rgb.output_override_active());
        assert!(!rgb.night_mode_active);
    }
}

#[test]
fn newly_unconfigured_output_releases_the_global_blackout() {
    let (mut rgb, received) = controller();
    let saved = config(false);
    rgb.apply_config(&saved, &[]);
    received.recv().unwrap();
    rgb.set_night_mode(true).unwrap();
    received.recv().unwrap();
    let (effects, other) = mpsc::channel();
    let existing = rgb.clone_wired_device("device").unwrap();
    rgb.replace_wired(HashMap::from([
        ("device".into(), existing),
        (
            "unconfigured".into(),
            Arc::new(RecordingRgb {
                effects,
                sync_calls: Default::default(),
            }) as Arc<dyn RgbDevice>,
        ),
    ]));
    rgb.check_thermal_override();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        saved.devices[0].zones[0].effect
    );
    assert!(!rgb.output_override_active());
    assert!(other.try_recv().is_err());
    rgb.set_effect("unconfigured", 0, &RgbEffect::default())
        .unwrap();
    assert_eq!(other.recv().unwrap().mode, RgbMode::Static);
    assert!(rgb.night_mode_active);
}

#[test]
fn night_mode_overlays_config_and_restores_the_current_saved_effect() {
    let (mut rgb, received) = controller();
    let saved = config(false);
    rgb.apply_config(&saved, &[]);
    assert_eq!(received.recv().unwrap(), saved.devices[0].zones[0].effect);
    rgb.set_night_mode(true).unwrap();
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    rgb.set_night_mode(true).unwrap();
    assert!(received.try_recv().is_err());
    assert_eq!(rgb.config.as_ref(), Some(&saved));
    assert!(rgb.set_effect("device", 0, &RgbEffect::default()).is_err());
    assert!(rgb.set_direct_colors("device", 0, &[[255; 3]; 2]).is_err());
    assert!(rgb
        .set_rgb_frames("device", &[vec![[255; 3]; 2]], 50)
        .is_err());
    assert!(rgb.set_mb_rgb_sync("device", true).is_err());
    assert!(rgb.set_mb_rgb_sync("device", false).is_err());
    let mut changed = saved.clone();
    changed.devices[0].zones[0].effect.colors = vec![[2, 3, 4]];
    rgb.apply_config(&changed, &[]);
    assert!(received.try_recv().is_err());
    rgb.set_night_mode(false).unwrap();
    assert_eq!(received.recv().unwrap(), changed.devices[0].zones[0].effect);
    rgb.set_night_mode(false).unwrap();
    assert!(received.try_recv().is_err());
    assert_eq!(saved.devices[0].zones[0].effect.colors, [[7, 8, 9]]);
    assert_eq!(rgb.config, Some(changed));
}

#[test]
fn night_mode_survives_device_replacement_and_hardware_invalidation() {
    let (mut rgb, received) = controller();
    let saved = config(false);
    rgb.apply_config(&saved, &[]);
    received.recv().unwrap();
    rgb.set_night_mode(true).unwrap();
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    rgb.invalidate_hardware_state();
    rgb.apply_config(&saved, &[]);
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    let (effects, replaced) = mpsc::channel();
    rgb.replace_wired(HashMap::from([(
        "device".into(),
        Arc::new(RecordingRgb {
            effects,
            sync_calls: Default::default(),
        }) as Arc<dyn RgbDevice>,
    )]));
    rgb.apply_config(&saved, &[]);
    assert_eq!(replaced.recv().unwrap().mode, RgbMode::Off);
    rgb.set_night_mode(false).unwrap();
    assert_eq!(replaced.recv().unwrap(), saved.devices[0].zones[0].effect);
}

#[test]
fn openrgb_bypasses_rgb_night_mode_and_preserves_thermal_permissions() {
    let (mut rgb, received) = controller();
    let saved = config(true);
    rgb.apply_config(&saved, &[]);
    rgb.set_openrgb_active(true).unwrap();
    let generation = rgb.output_resume_generation();
    rgb.set_night_mode(true).unwrap();
    assert!(rgb.night_mode_active);
    assert!(!rgb.output_override_active());
    assert!(received.try_recv().is_err());
    *rgb.thermal_override.lock() = Some([255, 128, 0]);
    assert!(!rgb.check_thermal_override());
    assert!(rgb.thermal_override_active());
    assert!(received.try_recv().is_err());
    rgb.set_direct_colors("device", 0, &[[255; 3]; 2]).unwrap();
    received.recv().unwrap();
    rgb.apply_config(&saved, &[]);
    assert!(received.try_recv().is_err());
    rgb.set_night_mode(false).unwrap();
    assert_eq!(rgb.output_resume_generation(), generation);
    assert!(rgb.thermal_override_active());
    assert!(received.try_recv().is_err());
    assert!(rgb.set_effect("device", 0, &RgbEffect::default()).is_ok());
    assert_eq!(received.recv().unwrap().mode, RgbMode::Static);
    assert!(rgb.set_direct_colors("device", 0, &[[5; 3]; 2]).is_ok());
    assert_eq!(received.recv().unwrap().colors, [[5; 3]]);
    assert!(rgb.set_mb_rgb_sync("device", true).is_ok());
    assert!(rgb.set_mb_rgb_sync("device", false).is_ok());
    rgb.set_night_mode(true).unwrap();
    assert!(received.try_recv().is_err());
    *rgb.thermal_override.lock() = None;
    assert!(!rgb.check_thermal_override());
    assert!(received.try_recv().is_err());
    rgb.set_openrgb_active(false).unwrap();
    assert!(received.try_recv().is_err());
    rgb.set_night_mode(false).unwrap();
    assert!(rgb.is_openrgb_controlled());
    assert!(rgb.set_direct_colors("device", 0, &[[5; 3]; 2]).is_ok());
    assert_eq!(received.recv().unwrap().colors, [[5; 3]]);
    assert_eq!(rgb.config, Some(saved));
}

#[test]
fn disabling_night_mode_exposes_thermal_alert_without_openrgb_control() {
    let (mut rgb, received) = controller();
    let saved = config(false);
    rgb.apply_config(&saved, &[]);
    received.recv().unwrap();
    rgb.set_night_mode(true).unwrap();
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    *rgb.thermal_override.lock() = Some([255, 128, 0]);
    assert!(rgb.check_thermal_override());
    assert!(received.try_recv().is_err());
    rgb.set_night_mode(false).unwrap();
    let alert = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(alert.mode, RgbMode::Static);
    assert_eq!(alert.colors, [[255, 128, 0]]);
    assert!(rgb.set_effect("device", 0, &RgbEffect::default()).is_err());
    assert!(rgb.set_mb_rgb_sync("device", true).is_err());
    assert!(rgb.set_mb_rgb_sync("device", false).is_ok());
    rgb.set_openrgb_active(true).unwrap();
    assert!(!rgb.check_thermal_override());
    assert!(rgb.thermal_override_active());
    assert!(rgb.set_direct_colors("device", 0, &[[5; 3]; 2]).is_ok());
    assert_eq!(received.recv().unwrap().colors, [[5; 3]]);
    rgb.set_openrgb_active(false).unwrap();
    assert_eq!(received.recv().unwrap().colors, [[255, 128, 0]]);
    *rgb.thermal_override.lock() = None;
    assert!(!rgb.check_thermal_override());
    assert_eq!(received.recv().unwrap(), saved.devices[0].zones[0].effect);
    assert_eq!(rgb.config, Some(saved));
}

#[test]
fn night_mode_discards_stale_wireless_uploads_before_resync() {
    let (mut rgb, received) = controller();
    let saved = config(false);
    rgb.apply_config(&saved, &[]);
    received.recv().unwrap();
    rgb.uploads.insert(
        "wireless:old".into(),
        Arc::new(WirelessRgbUpload::new(&[vec![[255; 3]; 26]], 50, None).unwrap()),
    );
    rgb.set_night_mode(true).unwrap();
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    rgb.resync_wireless_effects();
    assert!(rgb.uploads.is_empty());
    rgb.apply_config(&saved, &[]);
    assert!(received.try_recv().is_err());
    assert!(rgb.output_override_active());
}

struct StreamingRgb {
    frames: mpsc::Sender<Vec<[u8; 3]>>,
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    block_normal: std::sync::atomic::AtomicBool,
}

impl RgbDevice for StreamingRgb {
    fn device_name(&self) -> String {
        "Software RGB".into()
    }
    fn supported_modes(&self) -> Vec<RgbMode> {
        vec![RgbMode::Off, RgbMode::Static, RgbMode::Direct]
    }
    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        vec![RgbZoneInfo {
            name: "Ring".into(),
            led_count: 60,
        }]
    }
    fn set_zone_effect(&self, _: u8, _: &RgbEffect) -> anyhow::Result<()> {
        anyhow::bail!("software output required")
    }
    fn software_frame_delivery(&self) -> Option<RgbFrameDelivery> {
        Some(RgbFrameDelivery::Streaming)
    }
    fn software_render_profile(&self) -> Option<lianli_shared::rgb::RgbRenderProfile> {
        Some(lianli_shared::rgb::RgbRenderProfile {
            family: lianli_shared::rgb::RgbRenderFamily::UniversalScreen,
            fan_count: 0,
            led_count: 60,
            right_attach: false,
        })
    }
    fn set_software_frames(&self, frames: &[Vec<[u8; 3]>], _: u16) -> anyhow::Result<()> {
        if frames[0].iter().any(|color| *color != [0; 3])
            && self.block_normal.swap(false, Ordering::Relaxed)
        {
            self.entered.send(())?;
            self.release.lock().recv_timeout(Duration::from_secs(2))?;
        }
        self.frames.send(frames[0].clone())?;
        Ok(())
    }
}

#[test]
fn queued_black_follows_inflight_rainbow_and_resumes_the_latest_config() {
    let (frames, received) = mpsc::channel();
    let (entered, waiting) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let mut rgb = RgbController::new(
        HashMap::from([(
            "device".into(),
            Arc::new(StreamingRgb {
                frames,
                entered,
                release: Mutex::new(blocked),
                block_normal: std::sync::atomic::AtomicBool::new(true),
            }) as Arc<dyn RgbDevice>,
        )]),
        None,
    );
    let mut saved = config(false);
    saved.devices[0].zones[0].effect.mode = RgbMode::Rainbow;
    rgb.validate_config(&saved).unwrap();
    rgb.apply_config(&saved, &[]);
    waiting.recv_timeout(Duration::from_secs(1)).unwrap();
    rgb.set_night_mode(true).unwrap();
    assert!(rgb.night_mode_active);
    assert!(rgb.native_night_mode_engaged());
    rgb.set_night_mode(true).unwrap();
    assert!(rgb.set_effect("device", 0, &RgbEffect::default()).is_err());
    assert!(rgb
        .set_rgb_frames("device", &[vec![[255; 3]; 60]], 50)
        .is_err());
    release.send(()).unwrap();
    let old = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(old.iter().any(|color| *color != [0; 3]));
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0; 3]; 60]
    );
    assert!(received.recv_timeout(Duration::from_millis(80)).is_err());
    let mut changed = saved.clone();
    changed.devices[0].zones[0].effect.mode = RgbMode::Static;
    changed.devices[0].zones[0].effect.colors = vec![[0, 0, 255]];
    rgb.validate_config(&changed).unwrap();
    rgb.apply_config(&changed, &[]);
    rgb.set_night_mode(true).unwrap();
    assert_eq!(rgb.config.as_ref(), Some(&changed));
    assert!(received.recv_timeout(Duration::from_millis(80)).is_err());
    rgb.set_night_mode(false).unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0, 0, 254]; 60]
    );
    rgb.set_night_mode(false).unwrap();
    assert!(received.recv_timeout(Duration::from_millis(80)).is_err());
    assert!(waiting.try_recv().is_err());
}

struct DeferredRgb {
    frames: mpsc::Sender<Vec<[u8; 3]>>,
    deferred: std::sync::atomic::AtomicBool,
}

impl RgbDevice for DeferredRgb {
    fn device_name(&self) -> String {
        "Deferred RGB".into()
    }
    fn supported_modes(&self) -> Vec<RgbMode> {
        vec![RgbMode::Off, RgbMode::Static]
    }
    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        vec![RgbZoneInfo {
            name: "Ring".into(),
            led_count: 2,
        }]
    }
    fn deferred_reason(&self) -> Option<String> {
        self.deferred.load(Ordering::Relaxed).then(|| "Busy".into())
    }
    fn software_frame_delivery(&self) -> Option<RgbFrameDelivery> {
        Some(RgbFrameDelivery::LoopUpload)
    }
    fn set_zone_effect(&self, _: u8, _: &RgbEffect) -> anyhow::Result<()> {
        anyhow::bail!("software output required")
    }
    fn set_software_animation(
        &self,
        frames: &[Vec<[u8; 3]>],
        _: lianli_shared::rgb::RgbPlaybackTiming,
    ) -> anyhow::Result<()> {
        if self.deferred.swap(false, Ordering::Relaxed) {
            return Err(lianli_devices::traits::RgbDeferred {
                retry_after: Duration::from_millis(50),
                reason: "busy",
            }
            .into());
        }
        self.frames.send(frames[0].clone())?;
        Ok(())
    }
}

#[test]
fn queued_black_retries_deferred_delivery_and_restores_software_config() {
    let (frames, received) = mpsc::channel();
    let device = Arc::new(DeferredRgb {
        frames,
        deferred: std::sync::atomic::AtomicBool::new(false),
    });
    let mut rgb = RgbController::new(
        HashMap::from([("device".into(), device.clone() as Arc<dyn RgbDevice>)]),
        None,
    );
    rgb.apply_config(&config(false), &[]);
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[7, 8, 9]; 2]
    );
    device.deferred.store(true, Ordering::Relaxed);
    rgb.set_night_mode(true).unwrap();
    assert!(rgb.night_mode_active);
    assert!(rgb.native_night_mode_engaged());
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0; 3]; 2]
    );
    rgb.set_night_mode(false).unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[7, 8, 9]; 2]
    );
}

#[test]
fn native_and_software_outputs_black_out_and_restore_together() {
    let (mut rgb, native) = controller();
    let (frames, received) = mpsc::channel();
    let device = Arc::new(DeferredRgb {
        frames,
        deferred: std::sync::atomic::AtomicBool::new(false),
    });
    let existing = rgb.clone_wired_device("device").unwrap();
    rgb.replace_wired(HashMap::from([
        ("device".into(), existing),
        ("software".into(), device as Arc<dyn RgbDevice>),
    ]));
    let mut saved = config(false);
    let mut software = saved.devices[0].clone();
    software.device_id = "software".into();
    saved.devices.push(software);
    rgb.apply_config(&saved, &[]);
    native.recv_timeout(Duration::from_secs(1)).unwrap();
    received.recv_timeout(Duration::from_secs(1)).unwrap();
    rgb.set_night_mode(true).unwrap();
    assert!(rgb.night_mode_active);
    assert!(rgb.native_night_mode_engaged());
    assert_eq!(
        native.recv_timeout(Duration::from_secs(1)).unwrap().mode,
        RgbMode::Off
    );
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0; 3]; 2]
    );
    assert!(rgb.set_effect("device", 0, &RgbEffect::default()).is_err());
    let mut disabled = saved.clone();
    disabled.enabled = false;
    assert!(rgb.validate_config(&disabled).is_err());
    rgb.apply_config(&disabled, &[]);
    assert_eq!(rgb.config, Some(saved.clone()));
    rgb.set_night_mode(false).unwrap();
    assert_eq!(
        native.recv_timeout(Duration::from_secs(1)).unwrap(),
        saved.devices[0].zones[0].effect
    );
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[7, 8, 9]; 2]
    );
    assert!(native.try_recv().is_err());
    assert!(received.try_recv().is_err());
}

#[test]
fn configured_software_output_added_during_blackout_stays_dark_until_release() {
    let (mut rgb, native) = controller();
    let saved = config(false);
    rgb.apply_config(&saved, &[]);
    native.recv().unwrap();
    rgb.set_night_mode(true).unwrap();
    assert_eq!(native.recv().unwrap().mode, RgbMode::Off);
    let (frames, received) = mpsc::channel();
    let existing = rgb.clone_wired_device("device").unwrap();
    rgb.replace_wired(HashMap::from([
        ("device".into(), existing),
        (
            "software".into(),
            Arc::new(DeferredRgb {
                frames,
                deferred: std::sync::atomic::AtomicBool::new(false),
            }) as Arc<dyn RgbDevice>,
        ),
    ]));
    let mut changed = saved.clone();
    changed.devices[0].zones[0].effect.colors = vec![[4, 5, 6]];
    let mut software = changed.devices[0].clone();
    software.device_id = "software".into();
    changed.devices.push(software);
    rgb.apply_config(&changed, &[]);
    assert_eq!(
        native.recv_timeout(Duration::from_secs(1)).unwrap().mode,
        RgbMode::Off
    );
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[0; 3]; 2]
    );
    assert!(rgb.night_mode_active);
    assert!(rgb.native_night_mode_engaged());
    assert_eq!(rgb.config.as_ref(), Some(&changed));
    rgb.set_night_mode(false).unwrap();
    assert_eq!(
        native.recv_timeout(Duration::from_secs(1)).unwrap(),
        changed.devices[0].zones[0].effect
    );
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        [[4, 5, 6]; 2]
    );
    assert!(native.try_recv().is_err());
    assert!(received.try_recv().is_err());
}

#[test]
fn openrgb_direct_delivery_continues_while_night_mode_is_bypassed() {
    let (mut rgb, received) = controller();
    rgb.apply_config(&config(true), &[]);
    rgb.set_night_mode(true).unwrap();
    assert!(received.try_recv().is_err());
    let generation = rgb.output_resume_generation();
    let rgb = Arc::new(Mutex::new(rgb));
    let buffer = Arc::new(Mutex::new(DirectColorBuffer::new()));
    buffer.lock().set("device".into(), 0, vec![[4, 5, 6]; 2]);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = start_direct_color_writer(rgb.clone(), buffer, stop.clone());
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .colors,
        [[4, 5, 6]]
    );
    rgb.lock().set_night_mode(false).unwrap();
    assert_eq!(rgb.lock().output_resume_generation(), generation);
    assert!(received.try_recv().is_err());
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}

#[test]
fn native_output_gate_orders_inflight_writes_before_the_override() {
    let (mut rgb, received) = controller();
    rgb.apply_config(&config(false), &[]);
    received.recv().unwrap();
    let device = rgb.clone_wired_device("device").unwrap();
    let gate = rgb.output_override.clone();
    let (entered, waiting) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        output_override::with_normal_output(&gate, || {
            entered.send(())?;
            blocked.recv_timeout(Duration::from_secs(2))?;
            device.set_zone_effect(0, &RgbEffect::default())
        })
    });
    waiting.recv_timeout(Duration::from_secs(1)).unwrap();
    let (done, completed) = mpsc::channel();
    let override_worker = std::thread::spawn(move || {
        let mut rgb = rgb;
        rgb.set_night_mode(true).unwrap();
        done.send(()).unwrap();
        rgb
    });
    assert!(completed.recv_timeout(Duration::from_millis(20)).is_err());
    release.send(()).unwrap();
    writer.join().unwrap().unwrap();
    let rgb = override_worker.join().unwrap();
    assert_eq!(received.recv().unwrap().mode, RgbMode::Static);
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    assert!(output_override::with_normal_output(&rgb.output_override, || Ok(())).is_err());
}

struct FallibleRgb {
    failed: std::sync::atomic::AtomicBool,
    attempts: std::sync::atomic::AtomicUsize,
    effects: mpsc::Sender<RgbEffect>,
}
impl RgbDevice for FallibleRgb {
    fn device_name(&self) -> String {
        "Fallible RGB".into()
    }
    fn supported_modes(&self) -> Vec<RgbMode> {
        vec![RgbMode::Off, RgbMode::Static]
    }
    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        vec![RgbZoneInfo {
            name: "Lighting".into(),
            led_count: 1,
        }]
    }
    fn set_zone_effect(&self, _: u8, effect: &RgbEffect) -> anyhow::Result<()> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        anyhow::ensure!(!self.failed.load(Ordering::Relaxed), "device disconnected");
        self.effects.send(effect.clone())?;
        Ok(())
    }
}

fn fallible_controller() -> (RgbController, Arc<FallibleRgb>, mpsc::Receiver<RgbEffect>) {
    let (effects, received) = mpsc::channel();
    let device = Arc::new(FallibleRgb {
        failed: std::sync::atomic::AtomicBool::new(false),
        attempts: std::sync::atomic::AtomicUsize::new(0),
        effects,
    });
    let rgb = RgbController::new(
        HashMap::from([("device".into(), device.clone() as Arc<dyn RgbDevice>)]),
        None,
    );
    (rgb, device, received)
}

#[test]
fn repeated_night_mode_requests_retry_failed_delivery() {
    let (mut rgb, device, received) = fallible_controller();
    rgb.apply_config(&config(false), &[]);
    received.recv().unwrap();
    device.failed.store(true, Ordering::Relaxed);
    for attempt in 1..=2 {
        assert!(rgb
            .set_night_mode(true)
            .unwrap_err()
            .to_string()
            .contains("device disconnected"));
        assert!(rgb.output_override_active());
        assert!(rgb.set_effect("device", 0, &RgbEffect::default()).is_err());
        assert_eq!(device.attempts.load(Ordering::Relaxed), attempt + 1);
        rgb.check_thermal_override();
        assert_eq!(device.attempts.load(Ordering::Relaxed), attempt + 1);
    }
    device.failed.store(false, Ordering::Relaxed);
    rgb.set_night_mode(true).unwrap();
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    assert_eq!(device.attempts.load(Ordering::Relaxed), 4);
    rgb.set_night_mode(false).unwrap();
    assert_eq!(
        received.recv().unwrap(),
        config(false).devices[0].zones[0].effect
    );
    assert!(!rgb.output_override_active());
}

#[test]
fn failed_night_mode_restore_is_reported_and_can_be_retried() {
    let (mut rgb, device, received) = fallible_controller();
    let (effects, other) = mpsc::channel();
    rgb.replace_wired(HashMap::from([
        ("device".into(), device.clone() as Arc<dyn RgbDevice>),
        (
            "other".into(),
            Arc::new(RecordingRgb {
                effects,
                sync_calls: Default::default(),
            }) as Arc<dyn RgbDevice>,
        ),
    ]));
    let mut saved = config(false);
    let mut other_config = saved.devices[0].clone();
    other_config.device_id = "other".into();
    saved.devices.insert(0, other_config);
    rgb.apply_config(&saved, &[]);
    assert_eq!(received.recv().unwrap(), saved.devices[1].zones[0].effect);
    assert_eq!(other.recv().unwrap(), saved.devices[0].zones[0].effect);
    rgb.set_night_mode(true).unwrap();
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    assert_eq!(other.recv().unwrap().mode, RgbMode::Off);
    device.failed.store(true, Ordering::Relaxed);
    assert!(rgb
        .set_night_mode(false)
        .unwrap_err()
        .to_string()
        .contains("device disconnected"));
    assert!(!rgb.output_override_active());
    assert_eq!(other.recv().unwrap(), saved.devices[0].zones[0].effect);
    assert_eq!(device.attempts.load(Ordering::Relaxed), 3);
    rgb.check_thermal_override();
    assert_eq!(device.attempts.load(Ordering::Relaxed), 3);
    assert!(rgb.set_night_mode(false).is_err());
    assert_eq!(device.attempts.load(Ordering::Relaxed), 4);
    assert!(other.try_recv().is_err());
    device.failed.store(false, Ordering::Relaxed);
    rgb.set_night_mode(false).unwrap();
    assert_eq!(received.recv().unwrap(), saved.devices[1].zones[0].effect);
    assert_eq!(device.attempts.load(Ordering::Relaxed), 5);
    assert!(other.try_recv().is_err());
    assert!(rgb.override_error.is_none());
    rgb.set_night_mode(false).unwrap();
    assert!(received.try_recv().is_err());
    assert!(other.try_recv().is_err());
    assert_eq!(device.attempts.load(Ordering::Relaxed), 5);
    rgb.set_night_mode(true).unwrap();
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    assert_eq!(other.recv().unwrap().mode, RgbMode::Off);
    rgb.set_night_mode(false).unwrap();
    assert_eq!(received.recv().unwrap(), saved.devices[1].zones[0].effect);
    assert_eq!(other.recv().unwrap(), saved.devices[0].zones[0].effect);
    assert!(!rgb.output_override_active());
    assert!(rgb.override_error.is_none());
    assert_eq!(rgb.config, Some(saved));
}

#[test]
fn night_mode_off_preserves_thermal_permissions_for_ping_and_fan_direction() {
    let (mut rgb, received) = controller();
    rgb.apply_config(&config(false), &[]);
    received.recv().unwrap();
    *rgb.thermal_override.lock() = Some([255, 128, 0]);
    assert!(rgb.check_thermal_override());
    received.recv().unwrap();
    assert!(rgb.ping("device", 0).is_ok());
    assert!(rgb.set_fan_direction("device", 0, true, false).is_ok());
    assert!(rgb.set_effect("device", 0, &RgbEffect::default()).is_err());
    assert!(rgb.set_direct_colors("device", 0, &[[255; 3]; 2]).is_err());
    assert!(rgb.set_mb_rgb_sync("device", true).is_err());
    assert!(rgb.set_mb_rgb_sync("device", false).is_ok());
    rgb.set_night_mode(true).unwrap();
    assert_eq!(received.recv().unwrap().mode, RgbMode::Off);
    assert!(rgb.ping("device", 0).is_err());
    assert!(rgb.set_fan_direction("device", 0, true, false).is_err());
    rgb.set_night_mode(false).unwrap();
    assert_eq!(received.recv().unwrap().colors, [[255, 128, 0]]);
    assert!(rgb.ping("device", 0).is_ok());
    assert!(rgb.set_fan_direction("device", 0, true, false).is_ok());
}
