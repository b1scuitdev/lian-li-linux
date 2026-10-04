use super::controller::WirelessController;
use super::discovery::{poll_and_discover, DiscoveredDevice, RX_SLOT_LIMIT};
use super::{
    WirelessFanType, RF_CHUNKS, RF_CHUNK_SIZE, RF_DATA_SIZE, RF_PWM_CMD, RF_SELECT, USB_CMD_SEND_RF,
};
use anyhow::{bail, Context, Result};
use lianli_transport::usb::USB_TIMEOUT;
use std::thread;
use std::time::{Duration, Instant};
use tracing::info;

#[cfg(feature = "force-wireless-rebind")]
struct DiagnosticBindPlan {
    device: DiscoveredDevice,
    master: [u8; 6],
    channel: u8,
    rx: u8,
    outer_rx: u8,
    slot: u8,
    data: Vec<u8>,
}

impl WirelessController {
    #[cfg(feature = "force-wireless-rebind")]
    pub fn diagnostic_poll(&self, mac: [u8; 6]) -> Result<()> {
        anyhow::ensure!(
            self.poll_thread.is_none()
                && self.convergence_thread.is_none()
                && !self.rx_running.load(std::sync::atomic::Ordering::Acquire),
            "diagnostic requires an exclusive controller without workers"
        );
        let mut capture = self.receiver_state.diagnostic.lock();
        if capture.is_none() {
            *capture = Some(super::discovery::RawDiagnostic {
                target: mac,
                latest: None,
                sightings: 0,
                occupied: [false; RX_SLOT_LIMIT as usize],
                attempted: false,
            });
        }
        anyhow::ensure!(
            capture.as_ref().unwrap().target == mac,
            "diagnostic target changed"
        );
        drop(capture);
        poll_and_discover(
            self.rx.as_ref().context("RX not connected")?,
            &self.discovered_devices,
            &self.device_health,
            &self.master_entries,
            &self.receiver_state,
            &self.poll_stop,
            &self.master_mac,
        )
    }

    #[cfg(feature = "force-wireless-rebind")]
    fn diagnostic_bind_plan(&self, mac: &[u8; 6]) -> Result<DiagnosticBindPlan> {
        anyhow::ensure!(
            self.poll_thread.is_none()
                && self.convergence_thread.is_none()
                && !self.rx_running.load(std::sync::atomic::Ordering::Acquire),
            "diagnostic requires an exclusive controller without workers"
        );
        let master = *self.master_mac.lock();
        let channel = *self.master_channel.lock();
        anyhow::ensure!(
            master != [0; 6] && master != [0xff; 6] && (1..=39).contains(&channel),
            "valid current master MAC/channel unavailable"
        );
        let capture = self.receiver_state.diagnostic.lock();
        let capture = capture.as_ref().context("no raw diagnostic capture")?;
        anyhow::ensure!(
            capture.target == *mac && !capture.attempted && capture.sightings >= 3,
            "need at least three target sightings and no previous attempt"
        );
        let (raw, seen) = capture.latest.context("target not seen")?;
        anyhow::ensure!(
            seen.elapsed() <= super::discovery::ACK_FRESHNESS,
            "target raw sighting is stale"
        );
        anyhow::ensure!(
            raw[41] == 0x1c
                && raw[12] == 0
                && raw[18] == 0
                && (1..=4).contains(&raw[19])
                && raw[24..28].iter().any(|kind| (23..=26).contains(kind)),
            "target is not a malformed channel-zero SL V3 LCD fan group"
        );
        let raw_master: [u8; 6] = raw[6..12].try_into().unwrap();
        anyhow::ensure!(
            raw_master == master || raw_master == [0; 6],
            "target reports a different master; refusing to claim it"
        );
        let healthy = self.devices();
        anyhow::ensure!(
            healthy.len() < 10,
            "at most ten wireless devices can be bound"
        );
        anyhow::ensure!(
            healthy.iter().all(|device| device.channel == channel),
            "healthy devices disagree with the current master channel"
        );
        let first_free = self.get_rx_unused()?;
        let health = self.device_health.lock();
        let rx = (first_free..RX_SLOT_LIMIT)
            .find(|rx| {
                !capture.occupied[usize::from(*rx)]
                    && health
                        .values()
                        .all(|h| h.raw_rx != *rx && h.published.rx_type != *rx)
            })
            .context("no safely unoccupied RX slot")?;
        drop(health);
        let device = DiscoveredDevice {
            mac: *mac,
            master_mac: raw_master,
            channel,
            rx_type: 0,
            device_type: raw[18],
            fan_count: raw[19],
            is_inf_right_attach: false,
            fan_types: raw[24..28].try_into().unwrap(),
            fan_rpms: [0; 4],
            current_pwm: raw[36..40].try_into().unwrap(),
            cmd_seq: raw[40],
            fan_type: WirelessFanType::Slv3Lcd,
            list_index: 0,
            coolant_temp_c: None,
            effect_index: [0; 4],
            is_sync_mb_light: false,
            is_pwm_line_on: false,
            bind_intent: false,
        };
        let slot = self.next_slot_index(&device);
        let data = build_bind_packet(&device, &master, rx, channel, slot);
        Ok(DiagnosticBindPlan {
            device,
            master,
            channel,
            rx,
            outer_rx: 0xff,
            slot,
            data,
        })
    }

    #[cfg(feature = "force-wireless-rebind")]
    pub fn diagnostic_force_rebind(&self, mac: &[u8; 6]) -> Result<bool> {
        let _binding = self.begin_binding(mac)?;
        let DiagnosticBindPlan {
            device,
            master,
            channel,
            rx,
            outer_rx,
            slot,
            data,
        } = self.diagnostic_bind_plan(mac)?;
        info!(target = %device.mac_str(), master = %hex::encode(master), channel, rx,
            slot, outer_rx = "0xFF", payload = %hex::encode(&data),
            "Force-rebind: sending exactly one RF transaction");
        self.receiver_state
            .diagnostic
            .lock()
            .as_mut()
            .unwrap()
            .attempted = true;
        let sent_at = Instant::now();
        {
            // Do not replay a partially transmitted diagnostic transaction.
            let tx = self.tx.as_ref().context("TX not connected")?.lock();
            self.send_rf_packet_addressed(tx.get()?, channel, outer_rx, &data)?;
        }
        info!("Force-rebind packet sent; observing without further binds");
        let mut converged = false;
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(500));
            self.diagnostic_poll(*mac)?;
            let matched = self.device_health.lock().get(mac).is_some_and(|h| {
                h.raw_seen >= sent_at
                    && h.raw_master == master
                    && h.raw_channel == channel
                    && h.raw_rx == rx
            });
            if matched && !converged {
                self.confirm_binding(mac, true);
                converged = true;
                info!(target = %device.mac_str(), channel, rx, "Force-rebind converged");
            }
        }
        Ok(converged
            && self
                .devices()
                .iter()
                .any(|d| d.mac == *mac && d.channel == channel && d.rx_type == rx))
    }

    pub fn bind_device(&self, mac: &[u8; 6]) -> Result<()> {
        let _binding = self.begin_binding(mac)?;
        self.check_bind_allowed(mac)?;
        let master_mac = *self.master_mac.lock();
        let new_rx = self.get_rx_unused()?;
        self.converge_bind_state(mac, &master_mac, new_rx)?;
        self.confirm_binding(mac, true);
        self.save_rf_config()
    }

    pub fn unbind_device(&self, mac: &[u8; 6]) -> Result<()> {
        let _binding = self.begin_binding(mac)?;
        self.check_unbind_allowed(mac)?;
        self.converge_bind_state(mac, &[0u8; 6], 0)?;
        self.confirm_binding(mac, false);
        self.forget_mb_rgb_target(mac);
        self.save_rf_config()
    }

    fn begin_binding(&self, mac: &[u8; 6]) -> Result<BindingGuard> {
        let _order = self.command_order.lock();
        anyhow::ensure!(
            self.picture_target.lock().is_none(),
            "Wait for the wireless image upload to finish before changing binding"
        );
        let mut binding = self.binding_mac.lock();
        anyhow::ensure!(
            binding.is_none(),
            "another wireless binding operation is pending"
        );
        *binding = Some(*mac);
        if let Some(queue) = &self.pending_commands {
            queue.lock().retain(|command| command.mac != *mac);
        }
        Ok(BindingGuard(std::sync::Arc::clone(&self.binding_mac)))
    }

    fn check_bind_allowed(&self, mac: &[u8; 6]) -> Result<()> {
        let (raw_master, dead, fan_type) = {
            let health = self.device_health.lock();
            let Some(h) = health.get(mac) else {
                return Ok(());
            };
            (h.raw_master, h.dead, h.published.fan_type)
        };

        if dead {
            bail!("device is offline");
        }

        let local = *self.master_mac.lock();
        if raw_master != [0u8; 6] && raw_master != local && self.foreign_master_online(&raw_master)
        {
            bail!(
                "device {:02x?} is bound to another controller that is currently online",
                mac
            );
        }

        let bound: Vec<WirelessFanType> = {
            let health = self.device_health.lock();
            health
                .iter()
                .filter(|(m, h)| **m != *mac && h.bind_intent && !h.dead)
                .map(|(_, h)| h.published.fan_type)
                .collect()
        };

        if bound.len() >= 10 {
            bail!("at most 10 wireless devices can be bound");
        }
        match fan_type {
            WirelessFanType::Strimer(_)
                if bound
                    .iter()
                    .filter(|t| matches!(t, WirelessFanType::Strimer(_)))
                    .count()
                    >= 3 =>
            {
                bail!("at most 3 strimer devices can be bound");
            }
            WirelessFanType::WaterBlock
                if bound
                    .iter()
                    .any(|t| matches!(t, WirelessFanType::WaterBlock)) =>
            {
                bail!("only one HydroShift II LCD-C can be bound");
            }
            WirelessFanType::WaterBlock2
                if bound
                    .iter()
                    .any(|t| matches!(t, WirelessFanType::WaterBlock2)) =>
            {
                bail!("only one HydroShift II LCD-S can be bound");
            }
            _ => {}
        }

        Ok(())
    }

    fn check_unbind_allowed(&self, mac: &[u8; 6]) -> Result<()> {
        let Some(raw_master) = self.observed_master_of(mac) else {
            return Ok(());
        };
        let local = *self.master_mac.lock();
        if raw_master != [0u8; 6] && raw_master != local && self.foreign_master_online(&raw_master)
        {
            bail!(
                "device {:02x?} belongs to another controller that is currently online",
                mac
            );
        }
        Ok(())
    }

    pub(super) fn converge_bind_state(
        &self,
        mac: &[u8; 6],
        target_master_mac: &[u8; 6],
        target_rx: u8,
    ) -> Result<()> {
        const CONVERGE_TIMEOUT: Duration = Duration::from_secs(5);
        const POLL_GAP: Duration = Duration::from_millis(150);

        let rx = self.rx.as_ref().context("RX not connected")?;
        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        let mut attempts = 0u32;
        loop {
            anyhow::ensure!(
                !self.poll_stop.load(std::sync::atomic::Ordering::Acquire),
                "wireless controller is stopping"
            );
            let sent_at = Instant::now();
            self.send_bind_packet(mac, target_master_mac, target_rx)?;
            attempts += 1;
            thread::sleep(POLL_GAP);

            let _ = poll_and_discover(
                rx,
                &self.discovered_devices,
                &self.device_health,
                &self.master_entries,
                &self.receiver_state,
                &self.poll_stop,
                &self.master_mac,
            );

            let observed = self
                .device_health
                .lock()
                .get(mac)
                .filter(|h| h.raw_seen >= sent_at)
                .map(|h| (h.raw_master, h.raw_rx, h.raw_channel));

            let master_ch = *self.master_channel.lock();
            let converged = match observed {
                Some((m, r, ch)) => {
                    &m == target_master_mac && r == target_rx && (target_rx == 0 || ch == master_ch)
                }
                None => false,
            };
            if converged {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "bind convergence for {:02x?} timed out after {attempts} attempt(s); observed={:?} ch={}",
                    mac,
                    observed.map(|o| (o.0, o.1)),
                    observed.map(|o| o.2).unwrap_or(0)
                );
            }
        }
    }

    pub(super) fn send_bind_packet(
        &self,
        mac: &[u8; 6],
        target_master_mac: &[u8; 6],
        target_rx: u8,
    ) -> Result<()> {
        let device = self
            .discovered_devices
            .lock()
            .iter()
            .find(|d| d.mac == *mac)
            .cloned()
            .context("device not found in discovery")?;

        let master_ch = *self.master_channel.lock();
        let slot = if target_rx == 0 {
            0
        } else {
            self.next_slot_index(&device)
        };

        let rf_data = build_bind_packet(&device, target_master_mac, target_rx, master_ch, slot);

        self.tx_recover(|handle| {
            for _ in 0..6 {
                // Recovery cannot rely on the old RX slot. The payload MAC selects the device.
                self.send_rf_packet_addressed(handle, device.channel, 0xFF, &rf_data)?;
                thread::sleep(Duration::from_millis(30));
            }
            Ok(())
        })?;

        let verb = if target_rx == 0 { "Unbind" } else { "Bind" };
        info!(
            "{} sent to {} ({}) rx={} ch={} slot={}",
            verb,
            device.mac_str(),
            device.fan_type.display_name(),
            target_rx,
            master_ch,
            slot,
        );
        Ok(())
    }

    // Reserve both observed and published slots while recovery is pending.
    fn get_rx_unused(&self) -> Result<u8> {
        let health = self.device_health.lock();
        for rx in 1..RX_SLOT_LIMIT {
            let in_use = health
                .values()
                .any(|h| h.bind_intent && !h.dead && (h.raw_rx == rx || h.published.rx_type == rx));
            if !in_use {
                return Ok(rx);
            }
        }
        bail!("no free RX slot: all slots on this dongle are in use")
    }

    pub(super) fn save_rf_config(&self) -> Result<()> {
        let master_mac = *self.master_mac.lock();
        let master_ch = *self.master_channel.lock();

        let mut rf_data = vec![0u8; RF_DATA_SIZE];
        rf_data[0] = RF_SELECT;
        rf_data[1] = 0x15; // SaveConfig
        rf_data[2..8].copy_from_slice(&[0xFF; 6]);
        rf_data[8..14].copy_from_slice(&master_mac);
        rf_data[14] = 0xFF;

        self.tx_recover(|handle| {
            for _ in 0..3 {
                for chunk_idx in 0..RF_CHUNKS as u8 {
                    let mut packet = vec![0u8; 64];
                    packet[0] = USB_CMD_SEND_RF;
                    packet[1] = chunk_idx;
                    packet[2] = master_ch;
                    packet[3] = 0xFF;
                    let start = chunk_idx as usize * RF_CHUNK_SIZE;
                    packet[4..64].copy_from_slice(&rf_data[start..start + RF_CHUNK_SIZE]);
                    handle
                        .write(&packet, USB_TIMEOUT)
                        .context("sending SaveConfig")?;
                    thread::sleep(Duration::from_millis(1));
                }
                thread::sleep(Duration::from_millis(200));
            }
            Ok(())
        })
    }
}

fn build_bind_packet(
    device: &DiscoveredDevice,
    master: &[u8; 6],
    rx: u8,
    channel: u8,
    slot: u8,
) -> Vec<u8> {
    let mut data = vec![0; RF_DATA_SIZE];
    data[0] = RF_SELECT;
    data[1] = RF_PWM_CMD;
    data[2..8].copy_from_slice(&device.mac);
    data[8..14].copy_from_slice(master);
    data[14] = rx;
    data[15] = channel;
    data[16] = slot;
    data[17..21].copy_from_slice(&device.current_pwm);
    data
}

struct BindingGuard(std::sync::Arc<parking_lot::Mutex<Option<[u8; 6]>>>);

#[cfg(all(test, feature = "force-wireless-rebind"))]
#[path = "force_rebind_tests.rs"]
mod force_rebind_tests;

impl Drop for BindingGuard {
    fn drop(&mut self) {
        *self.0.lock() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::super::discovery::{DeviceHealth, MasterEntry};
    use super::*;

    pub(super) fn controller_with(local: [u8; 6], foreign_online: bool) -> WirelessController {
        let c = WirelessController::new();
        *c.master_mac.lock() = local;
        if foreign_online {
            let mut masters = c.master_entries.lock();
            masters.insert(
                [7u8; 6],
                MasterEntry {
                    channel: 8,
                    last_seen: Instant::now(),
                },
            );
        }
        c
    }

    pub(super) fn seed_device(
        c: &WirelessController,
        mac: &[u8; 6],
        master: [u8; 6],
        intent: bool,
    ) {
        let rec = super::super::discovery::DiscoveredDevice {
            mac: *mac,
            master_mac: master,
            channel: 8,
            rx_type: 1,
            device_type: 0,
            fan_count: 3,
            is_inf_right_attach: false,
            fan_types: [0; 4],
            fan_rpms: [0; 4],
            current_pwm: [0; 4],
            cmd_seq: 0,
            fan_type: WirelessFanType::Slv3Led,
            list_index: 0,
            coolant_temp_c: None,
            effect_index: [0; 4],
            is_sync_mb_light: false,
            is_pwm_line_on: false,
            bind_intent: false,
        };
        let mut health = c.device_health.lock();
        let mut h = DeviceHealth::new(rec);
        h.raw_master = master;
        h.bind_intent = intent;
        health.insert(*mac, h);
    }

    #[test]
    fn bind_packet_preserves_separate_rx_and_sensor_group_index() {
        let c = controller_with([9; 6], false);
        let mac = [1, 2, 3, 4, 5, 6];
        seed_device(&c, &mac, [9; 6], true);
        let mut device = c.device_health.lock()[&mac].published.clone();
        device.current_pwm = [100, 150, 200, 0];
        let data = build_bind_packet(&device, &[9; 6], 7, 8, 2);
        assert_eq!(
            &data[..21],
            &[0x12, 0x10, 1, 2, 3, 4, 5, 6, 9, 9, 9, 9, 9, 9, 7, 8, 2, 100, 150, 200, 0]
        );
        assert_eq!(data.len(), RF_DATA_SIZE);
        assert!(data[21..].iter().all(|byte| *byte == 0));
        let unbind = build_bind_packet(&device, &[0; 6], 0, 8, 0);
        assert_eq!(&unbind[8..17], &[0, 0, 0, 0, 0, 0, 0, 8, 0]);
    }

    #[test]
    fn rx_allocation_fails_when_every_slot_is_reserved() {
        let c = controller_with([9; 6], false);
        for rx in 1..RX_SLOT_LIMIT {
            let mac = [rx; 6];
            seed_device(&c, &mac, [9; 6], true);
            c.device_health
                .lock()
                .get_mut(&mac)
                .unwrap()
                .published
                .rx_type = rx;
        }
        assert!(c.get_rx_unused().is_err());
    }

    #[test]
    fn pending_binding_is_exclusive_and_released_on_failure() {
        let c = controller_with([9; 6], false);
        let mac = [1, 2, 3, 4, 5, 6];
        let binding = c.begin_binding(&mac).unwrap();
        assert!(c.begin_binding(&[2; 6]).is_err());
        assert_eq!(*c.binding_mac.lock(), Some(mac));
        drop(binding);
        assert!(c.bind_device(&mac).is_err());
        assert!(c.binding_mac.lock().is_none());
    }

    #[test]
    fn get_rx_unused_skips_a_slot_still_live_via_published_rx_type() {
        let c = controller_with([9u8; 6], false);
        seed_device(&c, &[1, 2, 3, 4, 5, 6], [9u8; 6], true);
        assert_eq!(
            c.get_rx_unused().unwrap(),
            2,
            "slot 1 is still live via published.rx_type even though raw_rx disagrees"
        );
    }

    #[test]
    fn confirmed_unbind_is_published_immediately_and_prevents_auto_rebind() {
        let c = controller_with([9; 6], false);
        let mac = [1, 2, 3, 4, 5, 6];
        seed_device(&c, &mac, [9; 6], true);
        let mut health = c.device_health.lock();
        let h = health.get_mut(&mac).unwrap();
        c.discovered_devices.lock().push(h.published.clone());
        h.raw_master = [0; 6];
        h.raw_rx = 0;
        drop(health);
        c.confirm_binding(&mac, false);
        assert!(c.devices().is_empty());
        assert_eq!(c.discovered_devices.lock()[0].rx_type, 0);
        assert_eq!(c.unbound_devices().len(), 1);
        assert!(c.rebind_candidates().is_empty());
    }

    #[test]
    fn failed_binding_commands_preserve_prior_intent() {
        let c = controller_with([9; 6], false);
        let mac = [1, 2, 3, 4, 5, 6];
        seed_device(&c, &mac, [0; 6], false);
        c.device_health.lock().get_mut(&mac).unwrap().man_unbind = true;
        assert!(c.bind_device(&mac).is_err());
        let health = c.device_health.lock();
        assert!(!health[&mac].bind_intent);
        assert!(health[&mac].man_unbind);
        drop(health);
        seed_device(&c, &mac, [9; 6], true);
        assert!(c.unbind_device(&mac).is_err());
        let health = c.device_health.lock();
        assert!(health[&mac].bind_intent);
        assert!(!health[&mac].man_unbind);
    }

    #[test]
    fn bind_refused_while_foreign_master_online() {
        let c = controller_with([9u8; 6], true);
        seed_device(&c, &[1, 2, 3, 4, 5, 6], [7u8; 6], false);
        assert!(c.check_bind_allowed(&[1, 2, 3, 4, 5, 6]).is_err());
    }

    #[test]
    fn bind_allowed_when_foreign_master_offline() {
        let c = controller_with([9u8; 6], false);
        seed_device(&c, &[1, 2, 3, 4, 5, 6], [7u8; 6], false);
        assert!(c.check_bind_allowed(&[1, 2, 3, 4, 5, 6]).is_ok());
    }

    #[test]
    fn bind_allowed_for_own_and_masterless_devices() {
        let c = controller_with([9u8; 6], true);
        seed_device(&c, &[1, 2, 3, 4, 5, 6], [9u8; 6], false);
        seed_device(&c, &[2, 2, 3, 4, 5, 6], [0u8; 6], false);
        assert!(c.check_bind_allowed(&[1, 2, 3, 4, 5, 6]).is_ok());
        assert!(c.check_bind_allowed(&[2, 2, 3, 4, 5, 6]).is_ok());
    }

    #[test]
    fn bind_refused_for_dead_device() {
        let c = controller_with([9u8; 6], false);
        seed_device(&c, &[1, 2, 3, 4, 5, 6], [0u8; 6], false);
        c.device_health
            .lock()
            .get_mut(&[1, 2, 3, 4, 5, 6])
            .unwrap()
            .dead = true;
        assert!(c.check_bind_allowed(&[1, 2, 3, 4, 5, 6]).is_err());
    }

    #[test]
    fn bind_caps_enforced() {
        let c = controller_with([9u8; 6], false);
        for i in 0..10u8 {
            seed_device(&c, &[i, 2, 3, 4, 5, 6], [0u8; 6], true);
        }
        seed_device(&c, &[50, 2, 3, 4, 5, 6], [0u8; 6], false);
        assert!(c.check_bind_allowed(&[50, 2, 3, 4, 5, 6]).is_err());

        let c = controller_with([9u8; 6], false);
        for i in 0..3u8 {
            seed_device(&c, &[i, 2, 3, 4, 5, 6], [0u8; 6], true);
            c.device_health
                .lock()
                .get_mut(&[i, 2, 3, 4, 5, 6])
                .unwrap()
                .published
                .fan_type = WirelessFanType::Strimer(1);
        }
        seed_device(&c, &[50, 2, 3, 4, 5, 6], [0u8; 6], false);
        c.device_health
            .lock()
            .get_mut(&[50, 2, 3, 4, 5, 6])
            .unwrap()
            .published
            .fan_type = WirelessFanType::Strimer(1);
        assert!(c.check_bind_allowed(&[50, 2, 3, 4, 5, 6]).is_err());
    }

    #[test]
    fn unbind_refused_while_foreign_master_online() {
        let c = controller_with([9u8; 6], true);
        seed_device(&c, &[1, 2, 3, 4, 5, 6], [7u8; 6], false);
        assert!(c.check_unbind_allowed(&[1, 2, 3, 4, 5, 6]).is_err());
    }

    #[test]
    fn unbind_allowed_for_own_devices() {
        let c = controller_with([9u8; 6], true);
        seed_device(&c, &[1, 2, 3, 4, 5, 6], [9u8; 6], true);
        assert!(c.check_unbind_allowed(&[1, 2, 3, 4, 5, 6]).is_ok());
    }
}
