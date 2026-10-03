use super::*;
use std::collections::HashMap;

pub(super) type States = HashMap<String, Arc<Mutex<State>>>;

pub(super) fn capabilities(cap: &RgbDeviceCapabilities) -> RgbDeviceCapabilities {
    let mut cap = cap.clone();
    if !cap.supports_direct {
        for zone in &mut cap.zones {
            zone.led_count = u16::from(zone.led_count != 0);
        }
        cap.total_led_count = cap.zones.iter().map(|zone| zone.led_count).sum();
    }
    cap
}

pub(super) fn states(
    caps: &[RgbDeviceCapabilities],
    previous: &States,
    rgb: &RgbController,
) -> States {
    caps.iter()
        .map(|cap| {
            let state = previous
                .get(&cap.device_id)
                .filter(|state| state.lock().cap == capabilities(cap))
                .cloned()
                .unwrap_or_else(|| {
                    Arc::new(Mutex::new(State::from_saved(
                        cap.clone(),
                        &rgb.saved_zone_effects(&cap.device_id),
                    )))
                });
            (cap.device_id.clone(), state)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_devices::traits::RgbDevice;
    use lianli_shared::rgb::RgbZoneInfo;

    #[test]
    fn zone_only_devices_expose_and_deliver_one_colour_per_zone() {
        let cap: RgbDeviceCapabilities = serde_json::from_value(serde_json::json!({
            "device_id": "aio", "device_name": "Test AIO",
            "supported_modes": ["Static", "RainbowMorph"],
            "zones": [{"name":"Pump Head", "led_count":12}, {"name":"Fans", "led_count":24}],
            "total_led_count":36, "supports_direct":false, "supports_mb_rgb_sync":false,
            "supported_scopes":[]
        }))
        .unwrap();
        let mut state = State::new(cap.clone());
        assert_eq!(state.cap.total_led_count, 2);
        assert_eq!(
            state
                .cap
                .zones
                .iter()
                .map(|z| z.led_count)
                .collect::<Vec<_>>(),
            [1, 1]
        );
        let buffer = Mutex::new(DirectColorBuffer::new());
        state
            .apply_colors(&Command::Colors(vec![[255, 0, 0], [0, 0, 255]]), &buffer)
            .unwrap();
        let writes = buffer.lock().take_all();
        assert_eq!(writes["aio"][&0], [[255, 0, 0]]);
        assert_eq!(writes["aio"][&1], [[0, 0, 255]]);
        state
            .apply_colors(
                &Command::SingleColor {
                    led: 1,
                    color: [0, 255, 0],
                },
                &buffer,
            )
            .unwrap();
        assert_eq!(state.colors, [[255, 0, 0], [0, 255, 0]]);
        let writes = buffer.lock().take_all();
        assert_eq!(writes["aio"].len(), 1);
        assert_eq!(writes["aio"][&1], [[0, 255, 0]]);
        assert!(state
            .apply_colors(&Command::Colors(vec![[255; 3]; 36]), &buffer)
            .is_err());
        let mut direct = cap.clone();
        direct.supports_direct = true;
        assert_eq!(State::new(direct.clone()).colors.len(), 36);
        let effect = RgbEffect {
            mode: RgbMode::Direct,
            ..Default::default()
        };
        let restored = State::from_saved(direct, &[(0, effect.clone()), (1, effect)]);
        assert_eq!(restored.modes[0].direction, 0);
        assert_eq!(cap.total_led_count, 36);

        let effect = RgbEffect {
            mode: RgbMode::RainbowMorph,
            brightness: 2,
            speed: 0,
            colors: Vec::new(),
            ..Default::default()
        };
        let saved = State::from_saved(cap.clone(), &[(0, effect.clone()), (1, effect)]);
        assert_eq!(saved.modes[saved.active].name, "Rainbow Morph");
        assert_eq!(saved.modes[saved.active].brightness, 2);

        let mut cap = cap;
        cap.supported_modes.push(RgbMode::Breathing);
        let effect = RgbEffect {
            mode: RgbMode::Breathing,
            colors: vec![[255, 0, 0]],
            brightness: 2,
            speed: 1,
            direction: RgbDirection::CounterClockwise,
            ..Default::default()
        };
        let saved = State::from_saved(cap.clone(), &[(0, effect.clone()), (1, effect.clone())]);
        assert_eq!(saved.modes[saved.active].colors, [[255, 0, 0]]);
        assert_eq!(saved.modes[saved.active].direction, DIR_LEFT);
        let disabled = RgbEffect {
            disabled: true,
            ..effect
        };
        let saved = State::from_saved(cap, &[(0, disabled.clone()), (1, disabled)]);
        assert_eq!(saved.active, 0);
        assert_eq!(saved.colors, [[0; 3]; 2]);
    }

    struct FailingZones {
        calls: Arc<Mutex<Vec<u8>>>,
        all_fail: bool,
        zone_count: usize,
    }

    #[test]
    fn independent_zone_modes_preserve_palettes_follow_and_replay() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let device = Arc::new(FailingZones {
            calls: calls.clone(),
            all_fail: false,
            zone_count: 2,
        });
        let mut rgb = RgbController::new(
            HashMap::from([("test".into(), device as Arc<dyn RgbDevice>)]),
            None,
        );
        let cap = rgb.exposed_capabilities().remove(0);
        let mut state = State::new(cap.clone());
        assert_eq!(state.zones.len(), 2);
        let mut mode = state.zones[1].modes[1].clone();
        mode.colors = vec![[255, 0, 0]];
        mode.brightness = 2;
        mode.speed = 3;
        let command = Command::ZoneMode {
            zone: 1,
            selection: Some((1, mode.clone())),
        };
        let mut next = state.clone();
        next.apply_mode(&command, &mut rgb).unwrap();
        state.commit_mode(next);
        assert_eq!(*calls.lock(), [1]);
        assert_eq!(state.zones[0].active_mode, -1);
        assert_eq!(state.effective_mode(1), &mode);
        let buffer = Mutex::new(DirectColorBuffer::new());
        state
            .apply_colors(&Command::Colors(vec![[0, 0, 255]; 2]), &buffer)
            .unwrap();
        let writes = buffer.lock().take_all();
        assert_eq!(writes["test"].len(), 1);
        assert_eq!(writes["test"][&0], [[0, 0, 255]]);
        calls.lock().clear();
        state.replay(&mut rgb, &buffer).unwrap();
        assert_eq!(*calls.lock(), [1]);
        assert_eq!(state.effective_mode(1).colors, [[255, 0, 0]]);
        let mut next = state.clone();
        next.apply_mode(
            &Command::ZoneMode {
                zone: 1,
                selection: None,
            },
            &mut rgb,
        )
        .unwrap();
        state.commit_mode(next);
        assert_eq!(state.effective_mode(1).name, "Direct");
        state.queue_colors(&buffer, Some(1));
        assert_eq!(buffer.lock().take_all()["test"][&1], [[0, 0, 255]]);
        let saved = State::from_saved(
            cap,
            &[
                (0, RgbEffect::default()),
                (
                    1,
                    RgbEffect {
                        mode: RgbMode::Breathing,
                        colors: vec![[255, 0, 0]],
                        brightness: 2,
                        speed: 3,
                        ..Default::default()
                    },
                ),
            ],
        );
        assert_eq!(saved.effective_mode(0).name, "Direct");
        assert_eq!(saved.effective_mode(1).name, "Breathing");
        assert_eq!(saved.effective_mode(1).brightness, 2);
        assert_eq!(saved.effective_mode(1).speed, 3);
        rgb.stop();
    }
    impl RgbDevice for FailingZones {
        fn device_name(&self) -> String {
            "Test".into()
        }
        fn supported_modes(&self) -> Vec<RgbMode> {
            vec![RgbMode::Breathing]
        }
        fn zone_info(&self) -> Vec<RgbZoneInfo> {
            vec![
                RgbZoneInfo {
                    name: "Zone".into(),
                    led_count: 1
                };
                self.zone_count
            ]
        }
        fn set_zone_effect(&self, zone: u8, _: &RgbEffect) -> anyhow::Result<()> {
            self.calls.lock().push(zone);
            anyhow::ensure!(zone != 0 && !self.all_fail, "test write failure");
            Ok(())
        }
    }

    #[test]
    fn partial_mode_delivery_attempts_every_zone_and_preserves_concurrent_colors() {
        for all_fail in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let device = Arc::new(FailingZones {
                calls: calls.clone(),
                all_fail,
                zone_count: 2,
            });
            let mut rgb = RgbController::new(
                HashMap::from([("test".into(), device as Arc<dyn RgbDevice>)]),
                None,
            );
            let mut state = State::new(rgb.exposed_capabilities().remove(0));
            let mut next = state.clone();
            state
                .apply_colors(
                    &Command::Colors(vec![[1, 2, 3]; 2]),
                    &Mutex::new(DirectColorBuffer::new()),
                )
                .unwrap();
            let mode = Command::Mode {
                index: 1,
                mode: next.modes[1].clone(),
            };
            let result = next.apply_mode(&mode, &mut rgb);
            assert_eq!(*calls.lock(), [0, 1]);
            assert_eq!(result.is_err(), all_fail);
            if result.is_ok() {
                state.commit_mode(next);
            }
            assert_eq!(state.active, usize::from(!all_fail));
            assert_eq!(state.colors, [[1, 2, 3]; 2]);
            assert_eq!(state.revision, if all_fail { 1 } else { 2 });
            rgb.stop();
        }
    }

    #[test]
    fn mode_on_device_without_zones_updates_state_without_hardware_writes() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let device = Arc::new(FailingZones {
            calls: calls.clone(),
            all_fail: true,
            zone_count: 0,
        });
        let mut rgb = RgbController::new(
            HashMap::from([("test".into(), device as Arc<dyn RgbDevice>)]),
            None,
        );
        let mut state = State::new(rgb.exposed_capabilities().remove(0));
        let mut next = state.clone();
        let command = Command::Mode {
            index: 1,
            mode: next.modes[1].clone(),
        };
        next.apply_mode(&command, &mut rgb).unwrap();
        state.commit_mode(next);
        assert_eq!(state.active, 1);
        assert_eq!(state.revision, 1);
        assert!(calls.lock().is_empty());
        rgb.stop();
    }
}

#[derive(Clone)]
pub(super) struct State {
    cap: RgbDeviceCapabilities,
    modes: Vec<ModeData>,
    active: usize,
    zones: Vec<ZoneModeData>,
    colors: Vec<[u8; 3]>,
    revision: u64,
}

impl State {
    fn from_saved(cap: RgbDeviceCapabilities, effects: &[(u8, RgbEffect)]) -> Self {
        let mut state = Self::new(cap);
        for (zone, effect) in effects {
            if let Ok(range) = state.zone_range(usize::from(*zone)) {
                let off = effect.disabled
                    || effect.mode == RgbMode::Off
                    || lianli_shared::rgb::is_brightness_off(effect.brightness);
                for (index, color) in state.colors[range].iter_mut().enumerate() {
                    *color = if off {
                        [0; 3]
                    } else {
                        effect
                            .colors
                            .get(index)
                            .or_else(|| effect.colors.first())
                            .copied()
                            .unwrap_or([0; 3])
                    };
                }
            }
        }
        if let Some((_, effect)) = effects.first().filter(|(_, first)| {
            effects.len() == state.cap.zones.len()
                && effects.iter().all(|(_, effect)| effect == first)
        }) {
            state.active = Self::restore_mode(&mut state.modes, effect);
        } else {
            for (zone, effect) in effects {
                if let Some(zone) = state.zones.get_mut(usize::from(*zone)) {
                    zone.active_mode = Self::restore_mode(&mut zone.modes, effect) as i32;
                }
            }
        }
        state
    }

    fn restore_mode(modes: &mut [ModeData], effect: &RgbEffect) -> usize {
        if effect.disabled || lianli_shared::rgb::is_brightness_off(effect.brightness) {
            return 0;
        }
        if let Some(index) = modes
            .iter()
            .position(|mode| mode.name == effect.mode.display_name())
        {
            let mode = &mut modes[index];
            if mode.color_mode == COLOR_MODE_MODE_SPECIFIC {
                if !(mode.colors_min as usize..=mode.colors_max as usize)
                    .contains(&effect.colors.len())
                {
                    return 0;
                }
                mode.colors.clone_from(&effect.colors);
            }
            mode.speed = u32::from(effect.speed).min(mode.speed_max);
            mode.brightness = u32::from(effect.brightness).min(mode.brightness_max);
            mode.direction =
                if mode.flags & (MODE_FLAG_HAS_DIRECTION_LR | MODE_FLAG_HAS_DIRECTION_UD) == 0 {
                    0
                } else {
                    match effect.direction {
                        RgbDirection::Clockwise | RgbDirection::Spread | RgbDirection::Gather => {
                            DIR_RIGHT
                        }
                        RgbDirection::CounterClockwise => DIR_LEFT,
                        RgbDirection::Up => DIR_UP,
                        RgbDirection::Down => DIR_DOWN,
                    }
                };
            return index;
        }
        0
    }

    pub fn replay(
        &self,
        rgb: &mut RgbController,
        buffer: &Mutex<DirectColorBuffer>,
    ) -> anyhow::Result<()> {
        if self.revision == 0 {
            return Ok(());
        }
        let mut failed = 0;
        for zone in 0..self.cap.zones.len() {
            if let Err(error) = self.deliver_mode(zone, self.effective_mode(zone), rgb) {
                failed += 1;
                warn!(device = %self.cap.device_id, zone, %error, "OpenRGB zone replay failed");
            }
        }
        self.queue_colors(buffer, None);
        anyhow::ensure!(failed == 0, "Some OpenRGB zones could not be replayed");
        Ok(())
    }

    pub fn new(cap: RgbDeviceCapabilities) -> Self {
        let cap = capabilities(&cap);
        let modes = ControllerSerializer {
            protocol_version: 6,
        }
        .build_modes(&cap);
        let colors = vec![[0; 3]; cap.total_led_count as usize];
        let zones = if cap.zone_effect_modes.is_empty() {
            Vec::new()
        } else {
            let mut zone_cap = cap.clone();
            zone_cap.supported_modes.clone_from(&cap.zone_effect_modes);
            let zone_modes = ControllerSerializer {
                protocol_version: 6,
            }
            .build_modes(&zone_cap);
            cap.zones
                .iter()
                .map(|_| ZoneModeData {
                    active_mode: -1,
                    modes: zone_modes.clone(),
                })
                .collect()
        };
        Self {
            cap,
            modes,
            active: 0,
            zones,
            colors,
            revision: 0,
        }
    }

    pub fn description(&self, version: u32) -> Vec<u8> {
        ControllerSerializer {
            protocol_version: version,
        }
        .build_controller_zones(
            &self.cap,
            &self.modes,
            self.active as u32,
            &self.colors,
            Some(&self.zones),
        )
    }

    pub fn apply_mode(&mut self, command: &Command, rgb: &mut RgbController) -> anyhow::Result<()> {
        match command {
            Command::Custom => {
                rgb.set_openrgb_active(true)?;
                self.active = 0;
                for zone in &mut self.zones {
                    zone.active_mode = -1;
                }
            }
            Command::ZoneMode { zone, selection } => {
                let target = self
                    .zones
                    .get_mut(*zone)
                    .ok_or_else(|| anyhow::anyhow!("Zone does not support independent modes"))?;
                let mode = if let Some((index, mode)) = selection {
                    let selected = target
                        .modes
                        .get_mut(*index)
                        .ok_or_else(|| anyhow::anyhow!("Unknown zone mode"))?;
                    Self::select_mode(selected, mode)?;
                    target.active_mode = *index as i32;
                    selected.clone()
                } else {
                    target.active_mode = -1;
                    self.modes[self.active].clone()
                };
                self.deliver_mode(*zone, &mode, rgb)?;
            }
            Command::Mode { index, mode } => {
                let selected = self
                    .modes
                    .get_mut(*index)
                    .ok_or_else(|| anyhow::anyhow!("Unknown device mode"))?;
                Self::select_mode(selected, mode)?;
                let selected = selected.clone();
                if selected.name != "Direct" {
                    let mut failed = 0;
                    for zone in 0..self.cap.zones.len() {
                        if let Err(error) = self.deliver_mode(zone, &selected, rgb) {
                            failed += 1;
                            warn!(device = %self.cap.device_id, zone, %error, "OpenRGB mode delivery failed");
                        }
                    }
                    anyhow::ensure!(
                        self.cap.zones.is_empty() || failed < self.cap.zones.len(),
                        "Mode delivery failed for every zone"
                    );
                    if failed > 0 {
                        warn!(device = %self.cap.device_id, failed, total = self.cap.zones.len(),
                            "OpenRGB mode only partially delivered; reporting the requested setting");
                    }
                }
                self.active = *index;
                for zone in &mut self.zones {
                    zone.active_mode = -1;
                }
            }
            _ => anyhow::bail!("Unsupported device mode command"),
        }
        Ok(())
    }

    fn select_mode(selected: &mut ModeData, mode: &ModeData) -> anyhow::Result<()> {
        anyhow::ensure!(selected.name == mode.name, "Device mode identity mismatch");
        anyhow::ensure!(
            mode.color_mode == selected.color_mode && mode.direction <= DIR_DOWN,
            "Invalid device mode parameters"
        );
        anyhow::ensure!(
            (selected.colors_min as usize..=selected.colors_max as usize)
                .contains(&mode.colors.len()),
            "Invalid device palette size"
        );
        selected.speed = mode.speed.min(MAX_EFFECT_VALUE);
        selected.brightness = mode.brightness.min(MAX_EFFECT_VALUE);
        selected.direction =
            if selected.flags & (MODE_FLAG_HAS_DIRECTION_LR | MODE_FLAG_HAS_DIRECTION_UD) == 0 {
                0
            } else {
                mode.direction
            };
        selected.colors.clone_from(&mode.colors);
        Ok(())
    }

    fn deliver_mode(
        &self,
        zone: usize,
        selected: &ModeData,
        rgb: &mut RgbController,
    ) -> anyhow::Result<()> {
        let effect = RgbEffect {
            mode: mode_from_openrgb_name(&selected.name, selected.value),
            colors: selected.colors.clone(),
            speed: selected.speed as u8,
            brightness: selected.brightness as u8,
            direction: match selected.direction {
                DIR_LEFT => RgbDirection::CounterClockwise,
                DIR_UP => RgbDirection::Up,
                DIR_DOWN => RgbDirection::Down,
                _ => RgbDirection::Clockwise,
            },
            ..Default::default()
        };
        if effect.mode != RgbMode::Direct {
            rgb.set_effect(&self.cap.device_id, zone as u8, &effect)?;
        }
        Ok(())
    }

    pub fn commit_mode(&mut self, next: Self) {
        // Colour updates can arrive while the mode write is waiting on the hardware.
        self.modes = next.modes;
        self.active = next.active;
        self.zones = next.zones;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn apply_colors(
        &mut self,
        command: &Command,
        buffer: &Mutex<DirectColorBuffer>,
    ) -> anyhow::Result<()> {
        match command {
            Command::Colors(colors) => {
                anyhow::ensure!(
                    colors.len() == self.colors.len(),
                    "Invalid device color count"
                );
                self.colors.clone_from(colors);
                self.queue_colors(buffer, None);
            }
            Command::ZoneColors { zone, colors } => {
                let range = self.zone_range(*zone)?;
                anyhow::ensure!(colors.len() == range.len(), "Invalid zone color count");
                self.colors[range].copy_from_slice(colors);
                self.queue_colors(buffer, Some(*zone));
            }
            Command::SingleColor { led, color } => {
                let target = self
                    .colors
                    .get_mut(*led)
                    .ok_or_else(|| anyhow::anyhow!("Invalid device LED"))?;
                *target = *color;
                let zone = (0..self.cap.zones.len())
                    .find(|zone| {
                        self.zone_range(*zone)
                            .is_ok_and(|range| range.contains(led))
                    })
                    .ok_or_else(|| anyhow::anyhow!("LED has no zone"))?;
                self.queue_colors(buffer, Some(zone));
            }
            _ => anyhow::bail!("Unsupported device command"),
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    fn zone_range(&self, zone: usize) -> anyhow::Result<std::ops::Range<usize>> {
        let info = self
            .cap
            .zones
            .get(zone)
            .ok_or_else(|| anyhow::anyhow!("Invalid device zone"))?;
        let start: usize = self.cap.zones[..zone]
            .iter()
            .map(|zone| zone.led_count as usize)
            .sum();
        let end = start + info.led_count as usize;
        anyhow::ensure!(end <= self.colors.len(), "Invalid device layout");
        Ok(start..end)
    }

    pub(super) fn queue_colors(&self, buffer: &Mutex<DirectColorBuffer>, selected: Option<usize>) {
        // Profile loading also sends stored colours after selecting a hardware effect.
        let mut buffer = buffer.lock();
        for zone in 0..self.cap.zones.len() {
            if self.effective_mode(zone).name == "Direct"
                && selected.is_none_or(|selected| selected == zone)
            {
                if let Ok(range) = self.zone_range(zone) {
                    buffer.set(
                        self.cap.device_id.clone(),
                        zone as u8,
                        self.colors[range].to_vec(),
                    );
                }
            }
        }
    }

    fn effective_mode(&self, zone: usize) -> &ModeData {
        self.zones
            .get(zone)
            .and_then(|state| {
                usize::try_from(state.active_mode)
                    .ok()
                    .and_then(|index| state.modes.get(index))
            })
            .unwrap_or(&self.modes[self.active])
    }

    pub fn update(&self, kind: u32) -> clients::Update {
        let description = if clients::is_color_update(kind) {
            Vec::new()
        } else {
            self.description(6)
        };
        clients::Update {
            revision: self.revision,
            data: clients::signal(kind, &self.colors, &description),
        }
    }
}
