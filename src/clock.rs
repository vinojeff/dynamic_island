//! Local wall-clock hour and minute.
//!
//! [`local_hm`] turns a Unix-epoch millisecond count into the local hour and
//! minute. The face that draws [`clock_label`] owns the font.

/// Hour and minute in the local timezone. The hour is `0..=23`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hm {
    /// `0..=23`.
    pub hour: u8,
    /// `0..=59`.
    pub minute: u8,
}

/// `HH:MM` in the local timezone.
#[must_use]
pub fn local_hm(unix_millis: u64) -> Hm {
    use chrono::{TimeZone, Timelike, Utc};
    let secs = i64::try_from(unix_millis / 1000).unwrap_or(i64::MAX);
    let Some(utc) = Utc.timestamp_opt(secs, 0).single() else {
        return Hm { hour: 0, minute: 0 };
    };
    let local = utc.with_timezone(&chrono::Local);
    #[allow(clippy::cast_possible_truncation)]
    Hm { hour: local.hour() as u8, minute: local.minute() as u8 }
}

/// `HH:MM` for the local timezone.
#[must_use]
pub fn clock_label(unix_millis: u64) -> String {
    let hm = local_hm(unix_millis);
    format!("{:02}:{:02}", hm.hour, hm.minute)
}

#[cfg(test)]
mod tests {
    use super::{clock_label, local_hm};

    #[test]
    fn the_label_matches_the_local_hour_and_minute() {
        let utc = (13 * 3600 + 5 * 60) * 1000;
        let hm = local_hm(utc);
        assert_eq!(clock_label(utc), format!("{:02}:{:02}", hm.hour, hm.minute));
        assert!(hm.hour < 24);
        assert!(hm.minute < 60);
    }
}
