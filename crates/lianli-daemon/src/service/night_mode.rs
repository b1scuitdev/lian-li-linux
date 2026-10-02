use super::ServiceManager;
use std::time::Duration;

impl ServiceManager {
    pub(super) fn set_night_mode(&mut self, enabled: bool) -> Result<(), String> {
        let mut targets = self
            .targets
            .try_lock_for(Duration::from_millis(100))
            .ok_or("LCD targets are busy. Retry shortly.")?;
        let mut rgb = self
            .controllers
            .rgb
            .as_ref()
            .map(|controller| {
                controller
                    .try_lock_for(Duration::from_millis(100))
                    .ok_or("RGB controller is busy. Retry shortly.")
            })
            .transpose()?;
        self.night_mode_active = enabled;
        let mut errors = Vec::new();
        if let Some(rgb) = &mut rgb {
            if let Err(error) = rgb.set_night_mode(enabled) {
                errors.push(format!("RGB: {error:#}"));
            }
        }
        for target in targets.values_mut() {
            if let Err(error) =
                target.set_night_mode(Some(&self.wireless), &mut self.packet_builder, enabled)
            {
                errors.push(format!("{}: {error}", target.device_identity));
            }
        }
        drop(rgb);
        drop(targets);
        self.ipc.state.lock().telemetry.night_mode_active = self.night_mode_active;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "Night Mode state changed, but lighting delivery failed: {}",
                errors.join("; ")
            ))
        }
    }

    pub(super) fn reapply_lcd_brightness(&mut self) {
        for target in self.targets.lock().values_mut() {
            target.reapply_brightness(Some(&self.wireless), &mut self.packet_builder);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn night_mode_telemetry_is_authoritative_and_configuration_is_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let mut service = ServiceManager::new(
            path.clone(),
            root.path().join("daemon.sock"),
            lianli_shared::daemon::DaemonMode::User,
        )
        .unwrap();
        let config = lianli_shared::config::AppConfig::default();
        service.config = Some(config.clone());
        service.ipc.state.lock().config = Some(config.clone());
        let before = serde_json::to_value(&config).unwrap();
        for enabled in [true, true, false, false] {
            service.set_night_mode(enabled).unwrap();
            assert_eq!(
                service.ipc.state.lock().telemetry.night_mode_active,
                enabled
            );
            service.sync_ipc_telemetry();
            assert_eq!(
                service.ipc.state.lock().telemetry.night_mode_active,
                enabled
            );
            assert_eq!(serde_json::to_value(&service.config).unwrap(), before);
            assert_eq!(
                serde_json::to_value(&service.ipc.state.lock().config).unwrap(),
                before
            );
            assert!(!path.exists());
        }
    }
}
