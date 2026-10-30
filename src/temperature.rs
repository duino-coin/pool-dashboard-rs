use std::path::Path;

/// Linux exposes CPU/SoC thermal sensors in millidegrees Celsius.
/// Unsupported or unreadable sensors remain absent rather than reporting 0°C.
pub fn cpu_temperature() -> Option<f64> {
    read_cpu_temperature(Path::new("/sys/class/thermal"))
}
fn read_cpu_temperature(root: &Path) -> Option<f64> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
        if ![
            "cpu-thermal",
            "bcm2835_thermal",
            "x86_pkg_temp",
            "soc_thermal",
        ]
        .contains(&kind.trim())
        {
            continue;
        }
        if let Some(value) = std::fs::read_to_string(path.join("temp"))
            .ok()
            .and_then(|text| text.trim().parse::<f64>().ok())
            .map(|value| value / 1000.)
            .filter(|value| value.is_finite() && (0.0..=150.0).contains(value))
        {
            return Some(value);
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_sensor_units_and_missing_or_invalid_values() {
        let root = std::env::temp_dir().join(format!("duino-thermal-{}", std::process::id()));
        let zone = root.join("thermal_zone0");
        std::fs::create_dir_all(&zone).unwrap();
        std::fs::write(zone.join("type"), "cpu-thermal\n").unwrap();
        std::fs::write(zone.join("temp"), "42842\n").unwrap();
        assert_eq!(read_cpu_temperature(&root), Some(42.842));
        for invalid in ["NaN", "invalid", "-1", "200000"] {
            std::fs::write(zone.join("temp"), invalid).unwrap();
            assert_eq!(read_cpu_temperature(&root), None);
        }
        std::fs::write(zone.join("type"), "gpu-thermal").unwrap();
        std::fs::write(zone.join("temp"), "50000").unwrap();
        assert_eq!(read_cpu_temperature(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(read_cpu_temperature(&root), None);
    }
}
