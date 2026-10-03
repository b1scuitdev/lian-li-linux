use super::*;
use lianli_shared::rgb::{RgbRenderFamily as Family, RgbRenderProfile};

const PROFILES: &[(Family, u8, u16)] = &[
    (Family::Tl, 3, 78),
    (Family::Sl, 3, 120),
    (Family::SlInf, 3, 132),
    (Family::SlInfV3, 3, 132),
    (Family::SlV4, 3, 156),
    (Family::Cl, 3, 72),
    (Family::P28, 3, 27),
    (Family::Strimer, 0, 116),
    (Family::HydroShiftII, 3, 96),
    (Family::HydroShiftII, 0, 24),
    (Family::HydroShiftIIOled, 0, 45),
    (Family::UniversalScreen, 0, 60),
    (Family::Lancool217, 0, 96),
    (Family::LancoolV150, 4, 88),
];

#[test]
fn continuous_blackout_preserves_mixed_wired_and_wireless_sync_delivery() {
    use lianli_devices::traits::RgbFrameDelivery;
    use std::cell::RefCell;

    struct SyncFan;
    impl RgbDevice for SyncFan {
        fn device_name(&self) -> String {
            "Sync fan".into()
        }
        fn supported_modes(&self) -> Vec<RgbMode> {
            vec![RgbMode::Static]
        }
        fn zone_info(&self) -> Vec<RgbZoneInfo> {
            vec![RgbZoneInfo {
                name: "Lighting".into(),
                led_count: 26,
            }]
        }
        fn software_render_profile(&self) -> Option<RgbRenderProfile> {
            Some(RgbRenderProfile {
                family: Family::Tl,
                fan_count: 1,
                led_count: 26,
                right_attach: false,
            })
        }
        fn software_frame_delivery(&self) -> Option<RgbFrameDelivery> {
            Some(RgbFrameDelivery::LoopUpload)
        }
        fn set_zone_effect(&self, _: u8, _: &RgbEffect) -> Result<()> {
            panic!("planning must not deliver RGB")
        }
    }
    let mut controller = RgbController::new(
        HashMap::from([("wired".into(), Arc::new(SyncFan) as Arc<dyn RgbDevice>)]),
        None,
    );
    controller.wireless_state.insert(
        "wireless".into(),
        WirelessDevice {
            mac: [1; 6],
            fan_type: WirelessFanType::SlV4,
            fan_count: 1,
            right_attach: false,
        },
    );
    let mut config = RgbAppConfig {
        merge_lighting: Some(MergeLightingConfig {
            enabled: true,
            device_order: vec!["wired".into(), "offline".into(), "wireless".into()],
            effect: RgbEffect {
                mode: RgbMode::Rainbow,
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    };
    for (blackout, blue) in [(false, false), (true, false), (false, true)] {
        if blue {
            config.merge_lighting.as_mut().unwrap().effect = RgbEffect {
                colors: vec![[0, 0, 255]],
                ..Default::default()
            };
        }
        let uploaded = RefCell::new(Vec::new());
        let plan = controller
            .prepare_sync_with(&config, blackout, None, |mac, animation, projected| {
                assert_eq!(mac, &[1; 6]);
                assert!(projected);
                assert!(animation.frames.iter().all(|frame| frame.len() == 52));
                uploaded.replace(animation.frames.clone());
                WirelessRgbUpload::with_timing(&animation.frames, animation.timing(), None)
            })
            .unwrap();
        assert_eq!(
            plan.iter().map(PreparedSync::id).collect::<Vec<_>>(),
            ["wired", "wireless"]
        );
        let PreparedSync::Wired { animation, .. } = &plan[0] else {
            panic!("wired delivery path lost")
        };
        let PreparedSync::Wireless { upload, .. } = &plan[1] else {
            panic!("wireless delivery path lost")
        };
        assert_eq!(usize::from(upload.frame_count()), uploaded.borrow().len());
        if blackout {
            assert_eq!(animation.frames.len(), 1);
            assert_eq!(upload.frame_count(), 1);
        }
        for frames in [&animation.frames, &*uploaded.borrow()] {
            assert!(frames.iter().all(|frame| !frame.is_empty()));
            if blackout {
                assert!(frames.iter().flatten().all(|color| *color == [0; 3]));
            } else {
                assert!(frames.iter().flatten().any(|color| *color != [0; 3]));
                if blue {
                    assert!(frames
                        .iter()
                        .flatten()
                        .all(|color| color[0] == 0 && color[1] == 0));
                }
            }
        }
    }
}

#[test]
fn native_sync_runway_uses_first_color_for_the_moving_stripe() {
    for &(family, fan_count, led_count) in PROFILES {
        for colors in [
            vec![[255, 0, 0], [0, 255, 0]],
            vec![[0, 255, 0], [255, 0, 0]],
        ] {
            let foreground = usize::from(colors[0][0] == 0);
            let background = 1 - foreground;
            let animation = native_animation(
                RgbRenderProfile {
                    family,
                    fan_count,
                    led_count,
                    right_attach: false,
                },
                &RgbEffect {
                    mode: RgbMode::Runway,
                    colors,
                    ..Default::default()
                },
            )
            .unwrap();
            let frame = &animation.frames[0];
            let moving = frame.iter().filter(|color| color[foreground] > 0).count();
            let resting = frame.iter().filter(|color| color[background] > 0).count();
            assert!(
                moving > 0 && moving < resting,
                "{family:?}: stripe={moving}, background={resting}"
            );
        }
    }
}

#[test]
fn native_sync_single_color_modes_do_not_introduce_other_colors() {
    for &(family, fan_count, led_count) in PROFILES {
        for mode in [RgbMode::Static, RgbMode::Breathing, RgbMode::Meteor] {
            let animation = native_animation(
                RgbRenderProfile {
                    family,
                    fan_count,
                    led_count,
                    right_attach: false,
                },
                &RgbEffect {
                    mode,
                    colors: vec![[255, 0, 0]],
                    ..Default::default()
                },
            )
            .unwrap();
            let pixels = animation.frames.iter().flatten().collect::<Vec<_>>();
            assert!(
                pixels.iter().any(|color| color[0] > 0),
                "{family:?} {mode:?}: no selected color"
            );
            assert!(
                pixels.iter().all(|color| color[1] == 0 && color[2] == 0),
                "{family:?} {mode:?}: unexpected palette color"
            );
        }
    }
}

#[test]
fn source_capacity_stack_survives_projection_and_upload_preparation() {
    let effect = RgbEffect {
        mode: RgbMode::Stack,
        colors: vec![[120, 30, 50]],
        ..Default::default()
    };
    let animation = sync_effects::render(&effect, 307).unwrap();
    assert_eq!(animation.frames.len(), 4082);
    let layout = Layout::for_profile(RgbRenderProfile {
        family: Family::Tl,
        fan_count: 1,
        led_count: 26,
        right_attach: false,
    })
    .unwrap();
    let frames = animation
        .frames
        .iter()
        .map(|frame| layout.project_frame(frame, 0..13, false).unwrap())
        .collect::<Vec<_>>();
    let upload = WirelessRgbUpload::with_timing(&frames, animation.timing(), None).unwrap();
    assert_eq!(upload.frame_count(), 4082);
    assert!(sync_effects::render(&effect, 308).is_err());
}

#[test]
fn native_sync_modes_cover_every_software_family() {
    for &(family, fan_count, led_count) in PROFILES {
        let profile = RgbRenderProfile {
            family,
            fan_count,
            led_count,
            right_attach: false,
        };
        for mode in MATCHED_MODES.iter().copied().chain([RgbMode::Twinkle]) {
            let effect = RgbEffect {
                mode,
                colors: vec![[255, 0, 0], [0, 255, 0]],
                ..Default::default()
            };
            let animation = native_animation(profile, &effect)
                .unwrap_or_else(|e| panic!("{family:?} {mode:?}: {e:#}"));
            assert!(!animation.frames.is_empty());
            assert!(
                animation
                    .frames
                    .iter()
                    .all(|frame| frame.len() == usize::from(led_count)),
                "{family:?} {mode:?}"
            );
        }
    }
}

#[test]
fn native_sync_rainbow_modes_ignore_custom_palette() {
    for &(family, fan_count, led_count) in PROFILES {
        let profile = RgbRenderProfile {
            family,
            fan_count,
            led_count,
            right_attach: false,
        };
        for mode in [RgbMode::Rainbow, RgbMode::RainbowMorph] {
            let mut effect = RgbEffect {
                mode,
                colors: vec![],
                ..Default::default()
            };
            let expected = native_animation(profile, &effect).unwrap();
            effect.colors = vec![[255, 0, 0], [0, 255, 0]];
            let actual = native_animation(profile, &effect).unwrap();
            assert_eq!(actual.frames, expected.frames, "{family:?} {mode:?}");
            assert_eq!(actual.interval_hundredths, expected.interval_hundredths);
        }
    }
}

#[test]
fn sync_configuration_rejects_duplicate_devices_and_invalid_controls() {
    let mut sync = MergeLightingConfig {
        enabled: true,
        device_order: vec!["a".into(), "a".into()],
        ..Default::default()
    };
    assert!(validate_settings(&sync).is_err());
    sync.device_order.pop();
    assert!(validate_settings(&sync).is_ok());
    sync.effect.brightness = 5;
    assert!(validate_settings(&sync).is_err());
    sync.effect.brightness = 255;
    assert!(validate_settings(&sync).is_ok());
    sync.effect.mode = RgbMode::Voice;
    assert!(validate_settings(&sync).is_err());
}
