use super::*;
use anyhow::Context;
use parking_lot::RwLock;

pub(super) fn with_normal_output(
    gate: &RwLock<Option<RgbEffect>>,
    write: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let guard = gate.read();
    anyhow::ensure!(guard.is_none(), "A runtime override currently controls RGB");
    write()
}

impl RgbController {
    pub fn set_night_mode(&mut self, enabled: bool) -> anyhow::Result<()> {
        self.night_mode_active = enabled;
        self.refresh_output_override(true)?;
        if let Some(error) = &self.override_error {
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    pub fn output_override_active(&self) -> bool {
        self.output_override.read().is_some()
    }

    pub fn output_resume_generation(&self) -> u64 {
        self.output_resume_generation
    }

    pub(super) fn ensure_night_mode_inactive(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.night_mode_active, "Night Mode currently controls RGB");
        Ok(())
    }

    pub(super) fn ensure_thermal_control_allowed(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.is_openrgb_controlled() || !self.thermal_override_active(),
            "thermal alert currently controls RGB"
        );
        Ok(())
    }

    pub(super) fn refresh_output_override(&mut self, retry_failed: bool) -> anyhow::Result<bool> {
        let effect = if self.night_mode_active {
            Some(RgbEffect {
                mode: RgbMode::Off,
                brightness: 0,
                colors: vec![[0; 3]],
                ..Default::default()
            })
        } else if self.is_openrgb_controlled() {
            None
        } else {
            self.thermal_override.lock().map(|color| RgbEffect {
                colors: vec![color],
                ..Default::default()
            })
        };
        let changed = *self.output_override.read() != effect;
        let revision = self.capabilities_revision.load(Ordering::Acquire);
        if !changed
            && (effect.is_none() || self.override_revision == Some(revision))
            && (!retry_failed || self.override_error.is_none())
        {
            return Ok(effect.is_some());
        }
        if changed {
            // Wait for native OpenRGB writes before replacing their physical output.
            *self.output_override.write() = effect.clone();
            self.capabilities_changed();
        }
        self.clear_pending();
        self.mb_sync_state.clear();
        if let Some(effect) = effect {
            self.override_revision = Some(self.capabilities_revision.load(Ordering::Acquire));
            if let Err(error) = self.apply_output_override(&effect) {
                self.override_error = Some(format!("{error:#}"));
                return Err(error);
            }
            Ok(true)
        } else {
            self.output_resume_generation = self.output_resume_generation.wrapping_add(1);
            if let Some(config) = self.config.clone() {
                if let Err(error) = self.apply_config_output(&config, &self.presets.clone(), None) {
                    self.override_error = Some(format!("{error:#}"));
                    return Err(error);
                }
            }
            Ok(false)
        }
    }

    fn apply_output_override(&mut self, effect: &RgbEffect) -> anyhow::Result<()> {
        let caps: Vec<_> = self
            .exposed_capabilities()
            .into_iter()
            .filter(|cap| {
                cap.total_led_count > 0
                    && (effect.mode != RgbMode::Off
                        || self.software_controlled(&cap.device_id)
                        || cap.supported_modes.contains(&RgbMode::Off)
                        || cap.supports_direct)
            })
            .collect();
        let mut errors = Vec::new();
        // Leaving motherboard sync can reset sibling ports, so do it before painting.
        for cap in &caps {
            if self.wired.contains_key(&cap.device_id) && cap.supports_mb_rgb_sync {
                if let Err(error) = self.apply_mb_rgb_sync(&cap.device_id, false) {
                    errors.push(format!("{}: {error:#}", cap.device_id));
                }
            }
        }
        for cap in caps {
            let result = if self.software_controlled(&cap.device_id) {
                let mut state = RenderState::new(
                    cap.zones
                        .iter()
                        .map(|zone| zone.led_count as usize)
                        .collect(),
                );
                (0..state.counts.len())
                    .try_for_each(|zone| state.set_effect(zone as u8, effect))
                    .and_then(|_| self.submit_render(&cap.device_id, &mut state))
            } else {
                let device = &self.wired[&cap.device_id];
                if effect.mode == RgbMode::Off && !cap.supported_modes.contains(&RgbMode::Off) {
                    cap.zones.iter().enumerate().try_for_each(|(zone, info)| {
                        device.set_direct_colors(zone as u8, &vec![[0; 3]; info.led_count as usize])
                    })
                } else {
                    device.set_all_effects(effect)
                }
            }
            .with_context(|| format!("RGB override for {}", cap.device_id));
            if let Err(error) = result {
                errors.push(format!("{error:#}"));
            }
        }
        anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
}
