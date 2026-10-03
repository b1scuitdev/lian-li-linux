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
        if self.native_night_mode_restore_pending {
            self.release_native_night_mode()?;
        }
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
        anyhow::ensure!(
            !self.native_night_mode_engaged(),
            "Night Mode currently controls RGB"
        );
        Ok(())
    }

    pub(super) fn native_night_mode_engaged(&self) -> bool {
        self.output_override
            .read()
            .as_ref()
            .is_some_and(|effect| effect.mode == RgbMode::Off)
    }

    pub(super) fn release_native_night_mode(&mut self) -> anyhow::Result<()> {
        if !self.native_night_mode_engaged() && !self.native_night_mode_restore_pending {
            return Ok(());
        }
        let enabled = std::mem::replace(&mut self.night_mode_active, false);
        let result = self.refresh_output_override(true).map(|_| ());
        self.night_mode_active = enabled;
        result
    }

    pub(super) fn ensure_thermal_control_allowed(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.is_openrgb_controlled() || !self.thermal_override_active(),
            "thermal alert currently controls RGB"
        );
        Ok(())
    }

    pub(super) fn refresh_output_override(&mut self, retry_failed: bool) -> anyhow::Result<bool> {
        let effect = if self.night_mode_active
            && !self.is_openrgb_controlled()
            && self.native_night_mode_eligible()
        {
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
        let night_transition = self.native_night_mode_engaged()
            || effect
                .as_ref()
                .is_some_and(|effect| effect.mode == RgbMode::Off);
        let changed = *self.output_override.read() != effect;
        let revision = self.capabilities_revision.load(Ordering::Acquire);
        if !changed
            && (effect.is_none() || self.override_revision == Some(revision))
            && (!retry_failed
                || (self.override_error.is_none() && !self.native_night_mode_restore_pending))
        {
            return Ok(effect.is_some());
        }
        if changed {
            self.native_night_mode_restore_pending |= self.native_night_mode_engaged();
            // Wait for native OpenRGB writes before replacing their physical output.
            *self.output_override.write() = effect.clone();
            self.capabilities_changed();
        }
        if changed || effect.is_some() {
            self.clear_pending();
            if !night_transition {
                self.mb_sync_state.clear();
            }
        } else {
            self.override_error = None;
        }
        if let Some(effect) = effect {
            self.override_revision = Some(self.capabilities_revision.load(Ordering::Acquire));
            if let Err(error) = self.apply_output_override(&effect) {
                self.override_error = Some(format!("{error:#}"));
                return Err(error);
            }
            self.native_night_mode_restore_pending = false;
            Ok(true)
        } else {
            if !night_transition && !self.native_night_mode_restore_pending {
                self.output_resume_generation = self.output_resume_generation.wrapping_add(1);
            }
            if let Some(config) = self.config.clone() {
                let result = self
                    .configure_fan_led_counts(&config)
                    .and_then(|()| self.apply_config_output(&config, &self.presets.clone(), None));
                if let Err(error) = result {
                    self.override_error = Some(format!("{error:#}"));
                    return Err(error);
                }
            }
            self.native_night_mode_restore_pending = false;
            Ok(false)
        }
    }

    fn apply_output_override(&mut self, effect: &RgbEffect) -> anyhow::Result<()> {
        let sync_ids = if effect.mode == RgbMode::Off {
            self.config
                .as_ref()
                .map(|config| self.sync_ids(config))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let caps: Vec<_> = self
            .exposed_capabilities()
            .into_iter()
            .filter(|cap| {
                !sync_ids.contains(&cap.device_id)
                    && cap.total_led_count > 0
                    && (effect.mode != RgbMode::Off
                        || self.software_controlled(&cap.device_id)
                        || cap.supported_modes.contains(&RgbMode::Off)
                        || cap.supports_direct)
            })
            .collect();
        let mut errors = Vec::new();
        for id in sync_ids {
            let result = self
                .config
                .as_ref()
                .context("RGB sync configuration unavailable")
                .and_then(|config| self.prepare_sync_blackout(config, &id))
                .and_then(|plan| plan.into_iter().try_for_each(|item| self.submit_sync(item)));
            if let Err(error) = result {
                errors.push(format!("RGB sync override for {id}: {error:#}"));
            }
        }
        if effect.mode != RgbMode::Off {
            // Leaving motherboard sync can reset sibling ports, so do it before painting.
            for cap in &caps {
                if self.wired.contains_key(&cap.device_id) && cap.supports_mb_rgb_sync {
                    if let Err(error) = self.apply_mb_rgb_sync(&cap.device_id, false) {
                        errors.push(format!("{}: {error:#}", cap.device_id));
                    }
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
