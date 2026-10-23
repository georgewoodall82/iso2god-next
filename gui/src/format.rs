//! Small presentation helpers shared by the widgets.

/// Formats a byte count using binary units, e.g. `7.3 GB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formats a transfer rate, e.g. `2.3 MiB/s`.
pub fn speed(bytes_per_sec: f64) -> String {
    format!("{}/s", size(bytes_per_sec.max(0.0) as u64))
}

/// Formats a time span coarsely, e.g. `45s`, `12m 05s` or `1h 12m`.
pub fn duration(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// Rounds a `0.0..=1.0` fraction to a whole percentage.
pub fn percent(fraction: f64) -> u32 {
    (fraction.clamp(0.0, 1.0) * 100.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_coarse_and_readable() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(60), "1m 00s");
        assert_eq!(duration(725), "12m 05s");
        assert_eq!(duration(3600), "1h 00m");
        assert_eq!(duration(4350), "1h 12m");
    }
}
