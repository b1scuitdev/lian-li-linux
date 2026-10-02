//! RGB effects, software rendering, and OpenRGB ownership.
mod capabilities;
mod configuration;
mod control;
mod direct_color;
mod output_override;
mod playback;
mod regions;
mod render;
mod strimer_sync;
mod sync_clock;
mod sync_device;
mod sync_plan;
#[cfg(test)]
mod sync_tests;
mod synchronization;
mod upload;
mod wired;

pub use direct_color::{start_direct_color_writer, DirectColorBuffer};

use lianli_devices::traits::RgbDevice;
use lianli_devices::wireless::{WirelessController, WirelessFanType, WirelessRgbUpload};
use lianli_media::rgb::FRAME_INTERVAL_MS;
use lianli_shared::rgb::{
    RgbAppConfig, RgbDeviceCapabilities, RgbEffect, RgbMode, RgbPreset, RgbPresetZone, RgbZoneInfo,
};
use render::RenderState;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};
use upload::{Command, UploadWorker};
use wired::WiredRenderer;

struct WirelessDevice {
    mac: [u8; 6],
    fan_count: u8,
    fan_type: WirelessFanType,
    right_attach: bool,
}

pub struct RgbController {
    wired: HashMap<String, Arc<dyn RgbDevice>>,
    wireless: Option<Arc<WirelessController>>,
    wireless_state: HashMap<String, WirelessDevice>,
    rendered: HashMap<String, RenderState>,
    applied: HashMap<String, RenderState>,
    configured: HashMap<String, String>,
    uploads: HashMap<String, Arc<WirelessRgbUpload>>,
    upload_worker: UploadWorker,
    wired_renderer: WiredRenderer,
    sync_clock: sync_clock::SyncClockWorker,
    sync_signature: Option<String>,
    sync_active: std::collections::HashSet<String>,
    config: Option<RgbAppConfig>,
    presets: Vec<RgbPreset>,
    openrgb_active: bool,
    openrgb_server_enabled: bool,
    thermal_override: crate::thermal_alert::SharedThermalAlert,
    night_mode_active: bool,
    output_override: Arc<parking_lot::RwLock<Option<RgbEffect>>>,
    override_revision: Option<u64>,
    override_error: Option<String>,
    output_resume_generation: u64,
    last_direct: HashMap<(String, u8), Vec<[u8; 3]>>,
    last_group_effects: HashMap<String, Vec<RgbEffect>>,
    capabilities_revision: Arc<AtomicU64>,
    delivery_generations: HashMap<String, u64>,
    mb_sync_state: HashMap<String, bool>,
    // Unrelated config saves must preserve colours pushed by live RGB clients.
    configured_direct_colors: HashMap<(String, u8), Vec<[u8; 3]>>,
}

impl RgbController {
    pub fn new(
        wired: HashMap<String, Arc<dyn RgbDevice>>,
        wireless: Option<Arc<WirelessController>>,
    ) -> Self {
        let mut controller = Self {
            wired,
            wireless,
            wireless_state: HashMap::new(),
            rendered: HashMap::new(),
            applied: HashMap::new(),
            configured: HashMap::new(),
            uploads: HashMap::new(),
            upload_worker: UploadWorker::new(),
            wired_renderer: WiredRenderer::new(),
            sync_clock: sync_clock::SyncClockWorker::new(),
            sync_signature: None,
            sync_active: Default::default(),
            config: None,
            presets: Vec::new(),
            openrgb_active: false,
            openrgb_server_enabled: false,
            thermal_override: crate::thermal_alert::new_shared(),
            night_mode_active: false,
            output_override: Arc::new(parking_lot::RwLock::new(None)),
            override_revision: None,
            override_error: None,
            output_resume_generation: 0,
            last_direct: HashMap::new(),
            last_group_effects: HashMap::new(),
            capabilities_revision: Arc::new(AtomicU64::new(0)),
            delivery_generations: HashMap::new(),
            mb_sync_state: HashMap::new(),
            configured_direct_colors: HashMap::new(),
        };
        controller.refresh_wireless_devices();
        controller
    }

    pub fn stop(&mut self) {
        self.sync_clock.stop();
        self.upload_worker.stop();
        self.wired_renderer.stop();
    }

    pub fn capabilities_revision(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.capabilities_revision)
    }

    pub fn delivery_device(&self, id: &str) -> Option<(Arc<dyn RgbDevice>, u64)> {
        self.wired.get(id).map(|device| {
            (
                device.clone(),
                self.delivery_generations.get(id).copied().unwrap_or(0),
            )
        })
    }

    fn capabilities_changed(&self) {
        self.capabilities_revision.fetch_add(1, Ordering::Release);
    }

    fn clear_pending(&mut self) {
        self.configured.clear();
        self.sync_clock.clear();
        self.sync_signature = None;
        self.sync_active.clear();
        self.upload_worker.clear();
        self.uploads.clear();
        if let Some(wireless) = &self.wireless {
            wireless.clear_rgb_targets();
        }
        self.wired_renderer.clear();
        self.applied.clear();
        self.override_revision = None;
        self.override_error = None;
    }

    pub fn set_thermal_override(&mut self, state: crate::thermal_alert::SharedThermalAlert) {
        self.thermal_override = state;
    }

    fn clear_device_pending(&mut self, id: &str) {
        self.configured.remove(id);
        self.applied.remove(id);
        self.wired_renderer.remove(id);
        if let Some(device) = self.wireless_state.get(id) {
            self.upload_worker.remove(&device.mac);
            if let (Some(wireless), Some(upload)) = (&self.wireless, self.uploads.get(id)) {
                wireless.forget_rgb_target(&device.mac, upload.effect_index());
            }
        }
        self.uploads.remove(id);
    }

    pub fn thermal_override_active(&self) -> bool {
        self.thermal_override.lock().is_some()
    }

    pub fn check_thermal_override(&mut self) -> bool {
        if let Err(error) = self.refresh_output_override(false) {
            warn!("RGB override failed: {error:#}");
        }
        !self.is_openrgb_controlled() && self.thermal_override_active()
    }

    pub fn software_controlled(&self, id: &str) -> bool {
        self.wired
            .get(id)
            .is_some_and(|d| d.software_frame_delivery().is_some() && !d.rf_owned())
            || self
                .wireless_state
                .get(id)
                .is_some_and(|d| d.fan_type.rgb_render_profile(d.fan_count).is_some())
    }

    fn render_state(&self, id: &str) -> anyhow::Result<RenderState> {
        let counts = if let Some(device) = self.wired.get(id) {
            device
                .zone_info()
                .iter()
                .map(|z| z.led_count as usize)
                .collect()
        } else if let Some(device) = self.wireless_state.get(id) {
            device.fan_type.rgb_zone_led_counts(device.fan_count)
        } else {
            anyhow::bail!("RGB device not found: {id}");
        };
        if let Some(state) = self.rendered.get(id).filter(|state| state.counts == counts) {
            return Ok(state.clone());
        }
        Ok(RenderState::new(counts))
    }

    fn render_profile(&self, id: &str) -> Option<lianli_shared::rgb::RgbRenderProfile> {
        self.wired
            .get(id)
            .and_then(|d| d.software_render_profile())
            .or_else(|| {
                self.wireless_state.get(id).and_then(|d| {
                    d.fan_type
                        .rgb_render_profile(d.fan_count)
                        .map(|mut profile| {
                            profile.right_attach = d.right_attach;
                            profile
                        })
                })
            })
    }

    fn regional_profile(&self, id: &str) -> Option<lianli_shared::rgb::RgbRenderProfile> {
        self.render_profile(id)
            .filter(|p| !lianli_media::rgb::family::modes(p.family).is_empty())
    }

    pub fn ping(&self, id: &str, zone: u8) -> anyhow::Result<()> {
        self.ensure_night_mode_inactive()?;
        if let Some(device) = self.wired.get(id) {
            return device.ping(zone);
        }
        let matched: Vec<_> = self
            .wired
            .iter()
            .filter(|(key, _)| key.starts_with(id))
            .collect();
        if !matched.is_empty() {
            for (_, device) in matched {
                device.ping(zone)?;
            }
            return Ok(());
        }
        if let (Some(wireless), Some(device)) = (&self.wireless, self.wireless_state.get(id)) {
            return wireless.selected_group(&device.mac);
        }
        anyhow::bail!("RGB device not found: {id}")
    }

    pub fn clone_wired_device(&self, id: &str) -> Option<Arc<dyn RgbDevice>> {
        self.wired.get(id).cloned()
    }

    pub fn is_openrgb_controlled(&self) -> bool {
        self.openrgb_active || self.openrgb_server_enabled
    }

    pub fn set_openrgb_active(&mut self, active: bool) {
        if self.openrgb_active == active {
            return;
        }
        self.openrgb_active = active;
        if self.output_override_active() {
            if let Err(error) = self.refresh_output_override(true) {
                warn!("RGB override failed: {error:#}");
            }
            return;
        }
        self.clear_pending();
        if !active && !self.openrgb_server_enabled {
            if let Some(config) = self.config.clone() {
                self.apply_config(&config, &self.presets.clone());
            }
        }
    }

    pub fn get_zone_colors(&self, id: &str, zone: u8) -> Option<Vec<[u8; 3]>> {
        let state = self.render_state(id).ok()?;
        Some(state.colors[state.range(zone).ok()?].to_vec())
    }

    pub fn get_all_zone_colors(&self, id: &str) -> Option<Vec<RgbPresetZone>> {
        self.software_controlled(id)
            .then(|| self.render_state(id).ok().map(|s| s.preset_zones()))
            .flatten()
    }

    pub fn get_effect_regions(&self, id: &str) -> Option<Vec<lianli_shared::rgb::RgbRegionConfig>> {
        if self.is_openrgb_controlled() {
            if let Some(effects) = self.last_group_effects.get(id) {
                return Some(
                    effects
                        .iter()
                        .cloned()
                        .map(|effect| lianli_shared::rgb::RgbRegionConfig {
                            effect,
                            flip: false,
                        })
                        .collect(),
                );
            }
        }
        self.rendered
            .get(id)
            .and_then(|state| state.regions.clone())
    }

    pub fn set_wireless(&mut self, wireless: Option<Arc<WirelessController>>) {
        self.capabilities_changed();
        self.sync_clock.clear();
        self.sync_signature = None;
        self.upload_worker.clear();
        if let Some(previous) = &self.wireless {
            previous.clear_rgb_targets();
        }
        self.wireless = wireless;
    }

    pub fn drain_wired(&mut self) -> HashMap<String, Arc<dyn RgbDevice>> {
        self.capabilities_changed();
        self.last_group_effects.clear();
        self.clear_pending();
        std::mem::take(&mut self.wired)
    }

    pub fn replace_wired(&mut self, wired: HashMap<String, Arc<dyn RgbDevice>>) {
        self.delivery_generations
            .retain(|id, _| wired.contains_key(id));
        self.capabilities_changed();
        self.last_group_effects.clear();
        self.sync_signature = None;
        self.configured
            .retain(|id, _| !self.wired.contains_key(id) && !wired.contains_key(id));
        self.wired = wired;
        self.rendered
            .retain(|id, _| self.wired.contains_key(id) || self.wireless_state.contains_key(id));
        self.last_direct.retain(|(id, _), _| {
            self.wired.contains_key(id) || self.wireless_state.contains_key(id)
        });
        self.configured_direct_colors.retain(|(id, _), _| {
            self.wired.contains_key(id) || self.wireless_state.contains_key(id)
        });
    }

    pub fn retain_wired(&mut self, present: &std::collections::HashSet<String>) {
        self.capabilities_changed();
        let previous: Vec<_> = self.wired.keys().cloned().collect();
        self.wired.retain(|id, _| {
            present.iter().any(|base| {
                id == base
                    || id
                        .strip_prefix(base)
                        .is_some_and(|suffix| suffix.starts_with(':'))
            })
        });
        for id in previous {
            if !self.wired.contains_key(&id) {
                self.delivery_generations.remove(&id);
                self.last_group_effects.remove(&id);
                self.sync_signature = None;
                self.sync_clock.clear();
                self.wired_renderer.remove(&id);
                self.applied.remove(&id);
                self.configured.remove(&id);
                self.rendered.remove(&id);
                self.mb_sync_state.remove(&id);
                self.last_direct.retain(|(device, _), _| device != &id);
                self.configured_direct_colors
                    .retain(|(device, _), _| device != &id);
            }
        }
    }

    pub fn refresh_wireless_devices(&mut self) {
        self.capabilities_changed();
        self.configured.retain(|id, _| !id.starts_with("wireless:"));
        self.sync_signature = None;
        let mut devices = HashMap::new();
        if let Some(wireless) = &self.wireless {
            for device in wireless.devices() {
                let id = format!("wireless:{}", device.mac_str());
                let counts = device.fan_type.rgb_zone_led_counts(device.fan_count);
                if self
                    .rendered
                    .get(&id)
                    .is_some_and(|state| state.counts != counts)
                {
                    self.rendered.remove(&id);
                }
                self.applied.remove(&id);
                self.uploads.remove(&id);
                self.mb_sync_state.remove(&id);
                devices.insert(
                    id,
                    WirelessDevice {
                        mac: device.mac,
                        fan_count: device.fan_count,
                        fan_type: device.fan_type,
                        right_attach: device.is_inf_right_attach,
                    },
                );
            }
        }
        self.wireless_state = devices;
        self.rendered
            .retain(|id, _| !id.starts_with("wireless:") || self.wireless_state.contains_key(id));
        self.applied
            .retain(|id, _| !id.starts_with("wireless:") || self.wireless_state.contains_key(id));
        self.uploads
            .retain(|id, _| self.wireless_state.contains_key(id));
        self.last_direct.retain(|(id, _), _| {
            self.wired.contains_key(id) || self.wireless_state.contains_key(id)
        });
        self.configured_direct_colors.retain(|(id, _), _| {
            self.wired.contains_key(id) || self.wireless_state.contains_key(id)
        });
    }

    pub fn wireless_topology_matches(
        &self,
        devices: &[lianli_devices::wireless::DiscoveredDevice],
    ) -> bool {
        devices.len() == self.wireless_state.len()
            && devices.iter().all(|device| {
                self.wireless_state
                    .get(&format!("wireless:{}", device.mac_str()))
                    .is_some_and(|old| {
                        old.fan_count == device.fan_count
                            && old.fan_type == device.fan_type
                            && old.right_attach == device.is_inf_right_attach
                    })
            })
    }

    pub fn invalidate_hardware_state(&mut self) {
        for id in self.wired.keys() {
            let generation = self.delivery_generations.entry(id.clone()).or_default();
            *generation = generation.wrapping_add(1);
        }
        self.capabilities_changed();
        self.last_group_effects.clear();
        self.clear_pending();
        self.mb_sync_state.clear();
    }

    pub fn invalidate_device_config(&mut self, id: &str) {
        if self.wired.contains_key(id) {
            let generation = self.delivery_generations.entry(id.to_owned()).or_default();
            *generation = generation.wrapping_add(1);
        }
        self.capabilities_changed();
        self.last_group_effects.remove(id);
        self.configured.remove(id);
    }
}

#[cfg(test)]
mod night_mode_tests;
#[cfg(test)]
mod tests;
