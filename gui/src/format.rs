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

/// Rounds a `0.0..=1.0` fraction to a whole percentage.
pub fn percent(fraction: f64) -> u32 {
    (fraction.clamp(0.0, 1.0) * 100.0).round() as u32
}
