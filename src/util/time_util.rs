//! Small time-formatting utilities.

/// Simple conversion from Unix seconds to calendar date. Not leap-second aware,
/// but accurate enough for human-readable generation listings.
pub fn format_timestamp(ts: u64) -> String {
    const DAYS_IN_MONTH: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut days = ts / 86400;
    let mut rem = ts % 86400;
    let hh = rem / 3600;
    rem %= 3600;
    let mm = rem / 60;
    let ss = rem % 60;

    let mut year = 1970u64;
    // A 400-year Gregorian cycle has exactly 146097 days. Process in large
    // chunks so that timestamps near u64::MAX do not loop billions of times.
    const DAYS_IN_400_YEARS: u64 = 146097;
    let cycles = days / DAYS_IN_400_YEARS;
    year += cycles * 400;
    days -= cycles * DAYS_IN_400_YEARS;
    loop {
        let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let year_days = if is_leap { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }

    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let mut month = 1u64;
    for (i, &dim) in DAYS_IN_MONTH.iter().enumerate() {
        let dim = if i == 1 && is_leap { 29 } else { dim };
        if days < dim {
            month = (i + 1) as u64;
            break;
        }
        days -= dim;
        month = (i + 2) as u64;
    }
    let day = days + 1;

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        year, month, day, hh, mm, ss
    )
}
