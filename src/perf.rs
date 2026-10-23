//! CPU, memory, and network samples for the performance page.
//!
//! `IslandView::arm` owns the loop. It warms one [`Sampler`] on the background
//! executor, waits [`SAMPLE_EVERY`], then each tick moves the sampler onto
//! that executor, calls [`Sampler::sample`], and moves it back. The `System`
//! is not rebuilt. CPU usage is a diff against the previous refresh, so a
//! fresh `System` reports 0.
//!
//! [`Sampler::sample`] calls `refresh_cpu_usage` and `refresh_memory` only,
//! then `networks.refresh(true)`. `net_up` / `net_down` are bytes since that
//! refresh, divided by the elapsed milliseconds. Loopback names (`lo`, `lo0`,
//! anything starting with `loopback`) are skipped.
//!
//! The view pushes the sample into [`PerfHistory`], four series capped at
//! [`HISTORY_CAP`] (60). Changing pages does not clear them. `cx.notify`
//! runs only while the performance tab is showing and the island is not a
//! line, so a hidden chart does not repaint the media page. Nothing here is
//! written to disk. GPU, disk, temperature, and the process list are not
//! sampled.

use std::collections::VecDeque;

use sysinfo::{Networks, System};

/// Points kept for each series. Older samples fall off the front.
pub const HISTORY_CAP: usize = 60;

/// How often a sample is taken.
pub const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_millis(1_000);

/// One performance sample. Rates are bytes per second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PerfSample {
    /// Global CPU usage, 0..=100.
    pub cpu: f32,
    /// Used memory as a percent of total, 0..=100.
    pub mem: f32,
    /// Used memory, in bytes.
    pub mem_bytes: u64,
    /// Bytes sent per second, excluding loopback.
    pub net_up: f32,
    /// Bytes received per second, excluding loopback.
    pub net_down: f32,
}

/// The four series the performance page draws.
#[derive(Clone, Debug, PartialEq)]
pub struct PerfHistory {
    /// CPU percent.
    pub cpu: VecDeque<f32>,
    /// Memory percent.
    pub mem: VecDeque<f32>,
    /// Upload, bytes per second.
    pub net_up: VecDeque<f32>,
    /// Download, bytes per second.
    pub net_down: VecDeque<f32>,
    /// Used memory of the newest sample, in bytes.
    pub mem_bytes: u64,
}

impl PerfHistory {
    /// Empty series.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cpu: VecDeque::new(),
            mem: VecDeque::new(),
            net_up: VecDeque::new(),
            net_down: VecDeque::new(),
            mem_bytes: 0,
        }
    }

    /// Append one sample. Each series keeps at most [`HISTORY_CAP`] points.
    pub fn push(&mut self, sample: PerfSample) {
        push_capped(&mut self.cpu, sample.cpu);
        push_capped(&mut self.mem, sample.mem);
        push_capped(&mut self.net_up, sample.net_up);
        push_capped(&mut self.net_down, sample.net_down);
        self.mem_bytes = sample.mem_bytes;
    }

    /// Newest CPU percent, or 0 when no sample has arrived.
    #[must_use]
    pub fn cpu_now(&self) -> f32 {
        self.cpu.back().copied().unwrap_or(0.0)
    }

    /// Newest memory percent, or 0 when no sample has arrived.
    #[must_use]
    pub fn mem_now(&self) -> f32 {
        self.mem.back().copied().unwrap_or(0.0)
    }
}

impl Default for PerfHistory {
    fn default() -> Self {
        Self::new()
    }
}

fn push_capped(series: &mut VecDeque<f32>, value: f32) {
    if series.len() == HISTORY_CAP {
        series.pop_front();
    }
    series.push_back(value);
}

/// Memory percent from used and total bytes. Zero total is 0%.
#[must_use]
pub fn memory_percent(used: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let percent = (used as f32 / total as f32) * 100.0;
    if percent.is_finite() { percent.clamp(0.0, 100.0) } else { 0.0 }
}

/// Windows loopback and the usual Unix names. sysinfo has no loopback flag.
#[must_use]
pub fn is_loopback(name: &str) -> bool {
    let name = name.trim();
    let lower = name.to_ascii_lowercase();
    lower == "lo"
        || lower == "lo0"
        || lower.starts_with("loopback")
}

/// Sum of received and transmitted bytes since the last refresh, skipping loopback.
#[must_use]
pub fn traffic(networks: &Networks) -> (u64, u64) {
    let mut down = 0u64;
    let mut up = 0u64;
    for (name, data) in networks {
        if is_loopback(name) {
            continue;
        }
        down = down.saturating_add(data.received());
        up = up.saturating_add(data.transmitted());
    }
    (down, up)
}

/// Bytes per second from a byte delta and the elapsed milliseconds.
#[must_use]
pub fn bytes_per_second(bytes: u64, elapsed_ms: u64) -> f32 {
    if elapsed_ms == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let rate = (bytes as f32) * 1000.0 / (elapsed_ms as f32);
    if rate.is_finite() { rate.max(0.0) } else { 0.0 }
}

/// Network chart ceiling: the next power of two at or above the peak, at least 64 KiB/s.
///
/// The peak is one of the points being drawn, so it never exceeds this ceiling.
/// The paint path also clamps the ratio to `0..=1`, so a spike stays inside the chart.
#[must_use]
pub fn network_ceiling(series: &[f32]) -> f32 {
    let peak = series.iter().copied().fold(0.0_f32, f32::max);
    let floor = 64.0 * 1024.0;
    if !peak.is_finite() || peak <= floor {
        return floor;
    }
    let exp = peak.log2().ceil();
    let raised = 2.0_f32.powf(exp);
    if raised.is_finite() { raised } else { floor }
}

/// Sixty slots, newest at the right. Missing samples stay 0.
///
/// A short history must not stretch across the chart: that draws a slope from
/// the first real sample down to zero. The real points keep their index and
/// slide left as new ones arrive.
#[must_use]
pub fn chart_slots(series: &[f32]) -> [f32; HISTORY_CAP] {
    let mut slots = [0.0; HISTORY_CAP];
    let skip = HISTORY_CAP.saturating_sub(series.len());
    for (index, value) in series.iter().enumerate() {
        let place = skip + index;
        if let Some(slot) = slots.get_mut(place) {
            *slot = *value;
        }
    }
    slots
}

/// Stroke color with `alpha` in `0..=1`.
#[must_use]
pub const fn tint(rgb: u32, alpha: f32) -> u32 {
    let alpha = if alpha.is_finite() { alpha.clamp(0.0, 1.0) } else { 0.0 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let channel = (alpha * 255.0) as u32;
    (channel << 24) | (rgb & 0x00ff_ffff)
}

/// `23%`.
#[must_use]
pub fn percent_label(value: f32) -> String {
    format!("{:.0}%", value.clamp(0.0, 100.0))
}

/// `12.4 GB` or `860 MB`.
#[must_use]
pub fn bytes_label(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f32;
    let gib = 1024.0 * 1024.0 * 1024.0;
    let mib = 1024.0 * 1024.0;
    if bytes >= gib {
        format!("{:.1} GB", bytes / gib)
    } else if bytes >= mib {
        format!("{:.0} MB", bytes / mib)
    } else {
        format!("{:.0} KB", bytes / 1024.0)
    }
}

/// `2.1 MB/s` or `340 KB/s`.
#[must_use]
pub fn rate_label(bytes_per_sec: f32) -> String {
    let rate = if bytes_per_sec.is_finite() { bytes_per_sec.max(0.0) } else { 0.0 };
    let mib = 1024.0 * 1024.0;
    if rate >= mib {
        format!("{:.1} MB/s", rate / mib)
    } else if rate >= 1024.0 {
        format!("{:.0} KB/s", rate / 1024.0)
    } else {
        format!("{rate:.0} B/s")
    }
}

/// Capsule line: the newest CPU, memory, and network sample.
///
/// Widths stay put across a refresh: CPU is three digits, memory is
/// `000.0 GB`, and each rate is `000.0` plus `KB/s` or `MB/s`.
/// Leading zeros are spaces. `CPU  23%  内存  12.4 GB  ↑   2.1 MB/s  ↓ 340.0 KB/s`
#[must_use]
pub fn capsule_line(history: &PerfHistory) -> String {
    format!(
        "CPU {}   \u{5185}\u{5b58} {}   \u{2191} {}   \u{2193} {}",
        cpu_capsule(history.cpu_now()),
        memory_capsule(history.mem_bytes),
        rate_capsule(history.net_up.back().copied().unwrap_or(0.0)),
        rate_capsule(history.net_down.back().copied().unwrap_or(0.0)),
    )
}

/// ` 23%`. The integer is always three columns; leading zeros are spaces.
fn cpu_capsule(value: f32) -> String {
    let percent = value.clamp(0.0, 100.0).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let percent = percent as u8;
    spaced(&format!("{percent:03}"), "%")
}

/// `012.4 GB`. The integer is three columns and the fraction is one digit.
fn memory_capsule(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let gib = (bytes as f32 / (1024.0 * 1024.0 * 1024.0)).clamp(0.0, 999.9);
    fixed_decimal(gib, " GB")
}

/// `002.1 MB/s` or `340.0 KB/s`. Three integer columns, one decimal.
fn rate_capsule(bytes_per_sec: f32) -> String {
    let rate = if bytes_per_sec.is_finite() { bytes_per_sec.max(0.0) } else { 0.0 };
    let mib = 1024.0 * 1024.0;
    if rate >= mib {
        fixed_decimal((rate / mib).clamp(0.0, 999.9), " MB/s")
    } else {
        fixed_decimal((rate / 1024.0).clamp(0.0, 999.9), " KB/s")
    }
}

/// ` 12.4` plus `suffix`. The integer is three columns; leading zeros are spaces.
fn fixed_decimal(value: f32, suffix: &str) -> String {
    let scaled = (value * 10.0).round().clamp(0.0, 9999.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled = scaled as u16;
    spaced(&format!("{:03}.{}", scaled / 10, scaled % 10), suffix)
}

/// Replace leading zeros with spaces. A zero digit after the decimal stays a zero.
fn spaced(digits: &str, suffix: &str) -> String {
    let mut seen = false;
    let mut text = String::with_capacity(digits.len() + suffix.len());
    for unit in digits.chars() {
        if unit == '0' && !seen {
            text.push(' ');
        } else {
            if unit != '.' {
                seen = true;
            }
            text.push(unit);
        }
    }
    text.push_str(suffix);
    text
}

/// One `System` and one `Networks`, kept across samples.
pub struct Sampler {
    system: System,
    networks: Networks,
    last_ms: Option<u64>,
}

impl Sampler {
    /// Build the sampler and take one throwaway refresh.
    ///
    /// CPU usage needs a previous refresh. The first real [`sample`](Self::sample)
    /// would otherwise report 0.
    #[must_use]
    pub fn warm(now_ms: u64) -> Self {
        let system = System::new_all();
        let networks = Networks::new_with_refreshed_list();
        let mut sampler = Self { system, networks, last_ms: Some(now_ms) };
        sampler.system.refresh_cpu_usage();
        sampler.system.refresh_memory();
        sampler.networks.refresh(true);
        sampler
    }

    /// Read one sample. `now_ms` is a monotonic millisecond clock.
    pub fn sample(&mut self, now_ms: u64) -> PerfSample {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.networks.refresh(true);
        let elapsed = self.last_ms.map_or(1_000, |then| now_ms.saturating_sub(then).max(1));
        self.last_ms = Some(now_ms);
        let (down, up) = traffic(&self.networks);
        PerfSample {
            cpu: self.system.global_cpu_usage().clamp(0.0, 100.0),
            mem: memory_percent(self.system.used_memory(), self.system.total_memory()),
            mem_bytes: self.system.used_memory(),
            net_up: bytes_per_second(up, elapsed),
            net_down: bytes_per_second(down, elapsed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HISTORY_CAP, PerfHistory, PerfSample, bytes_label, bytes_per_second, capsule_line, chart_slots,
        is_loopback, memory_percent, network_ceiling, percent_label, rate_label,
    };

    fn sample(cpu: f32, mem: f32, up: f32, down: f32) -> PerfSample {
        PerfSample { cpu, mem, mem_bytes: 0, net_up: up, net_down: down }
    }

    #[test]
    fn history_drops_the_oldest_point_past_sixty() {
        let mut history = PerfHistory::new();
        for index in 0..HISTORY_CAP + 3 {
            #[allow(clippy::cast_precision_loss)]
            history.push(sample(index as f32, 0.0, 0.0, 0.0));
        }
        assert_eq!(history.cpu.len(), HISTORY_CAP);
        assert_eq!(history.cpu.front().copied(), Some(3.0));
        #[allow(clippy::cast_precision_loss)]
        let newest = (HISTORY_CAP + 2) as f32;
        assert!((history.cpu_now() - newest).abs() < 0.01);
    }

    #[test]
    fn memory_percent_is_zero_when_the_total_is_unknown() {
        assert!(memory_percent(0, 0).abs() < 0.01);
        assert!((memory_percent(512, 1024) - 50.0).abs() < 0.01);
    }

    #[test]
    fn loopback_names_are_skipped() {
        assert!(is_loopback("Loopback Pseudo-Interface 1"));
        assert!(is_loopback("lo"));
        assert!(is_loopback("lo0"));
        assert!(!is_loopback("Ethernet"));
        assert!(!is_loopback("Wi-Fi"));
        assert!(!is_loopback("vEthernet (Default Switch)"));
    }

    #[test]
    fn a_one_second_delta_is_the_byte_rate() {
        assert!((bytes_per_second(2_048, 1_000) - 2_048.0).abs() < 0.01);
        assert!(bytes_per_second(9, 0).abs() < 0.01);
    }

    #[test]
    fn the_network_axis_steps_up_by_powers_of_two() {
        assert!((network_ceiling(&[0.0]) - 64.0 * 1024.0).abs() < 1.0);
        assert!((network_ceiling(&[90_000.0]) - 128.0 * 1024.0).abs() < 1.0);
        let spike = 10.0 * 1024.0 * 1024.0;
        assert!((network_ceiling(&[10.0 * 1024.0, spike]) - 16.0 * 1024.0 * 1024.0).abs() < 1.0);
    }

    #[test]
    fn labels_use_the_unit_that_fits() {
        assert_eq!(percent_label(23.4), "23%");
        assert_eq!(bytes_label(13_304_627_200), "12.4 GB");
        assert_eq!(rate_label(2.1 * 1024.0 * 1024.0), "2.1 MB/s");
        assert_eq!(rate_label(340.0 * 1024.0), "340 KB/s");
        let mut history = PerfHistory::new();
        history.push(PerfSample { cpu: 23.0, mem: 40.0, mem_bytes: 13_304_627_200, net_up: 2.1 * 1024.0 * 1024.0, net_down: 340.0 * 1024.0 });
        assert_eq!(
            capsule_line(&history),
            "CPU  23%   \u{5185}\u{5b58}  12.4 GB   \u{2191}   2.1 MB/s   \u{2193} 340.0 KB/s",
        );
    }

    #[test]
    fn a_short_series_sits_on_the_right_and_the_rest_stays_zero() {
        let slots = chart_slots(&[4.0, 9.0]);
        assert_eq!(slots.len(), HISTORY_CAP);
        assert!((slots[HISTORY_CAP - 1] - 9.0).abs() < 0.01);
        assert!((slots[HISTORY_CAP - 2] - 4.0).abs() < 0.01);
        assert!(slots[..HISTORY_CAP - 2].iter().all(|value| value.abs() < 0.01));
        let full = chart_slots(&[1.0; HISTORY_CAP]);
        assert!(full.iter().all(|value| (*value - 1.0).abs() < 0.01));
    }
}
