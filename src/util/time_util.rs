//! Small time-formatting utilities.

/// Formats Unix seconds as a UTC calendar timestamp for human-readable generation listings.
pub fn format_timestamp(ts: u64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("{ts} (epoch)"))
}
