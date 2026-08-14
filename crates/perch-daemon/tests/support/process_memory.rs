//! Linux proportional and private-dirty memory sampling for benchmark process
//! groups. Both values come from the same `smaps_rollup` reads so comparisons
//! do not mix different points in a workload.

#[cfg(target_os = "linux")]
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProcessGroupMemoryKib {
    pub(crate) pss: Option<u64>,
    pub(crate) private_dirty: Option<u64>,
}

#[cfg(target_os = "linux")]
pub(crate) fn median_group_memory_kib(pids: &[u32]) -> ProcessGroupMemoryKib {
    let mut pss_readings = Vec::with_capacity(7);
    let mut private_dirty_readings = Vec::with_capacity(7);
    for _ in 0..7 {
        let mut pss = 0;
        let mut private_dirty = 0;
        for pid in pids {
            let contents = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
                .expect("read process smaps_rollup");
            pss += parse_kib_field(&contents, "Pss").expect("smaps_rollup contains Pss");
            private_dirty += parse_kib_field(&contents, "Private_Dirty")
                .expect("smaps_rollup contains Private_Dirty");
        }
        pss_readings.push(pss);
        private_dirty_readings.push(private_dirty);
        std::thread::sleep(Duration::from_millis(25));
    }
    ProcessGroupMemoryKib {
        pss: Some(median(pss_readings)),
        private_dirty: Some(median(private_dirty_readings)),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn median_group_memory_kib(_pids: &[u32]) -> ProcessGroupMemoryKib {
    ProcessGroupMemoryKib::default()
}

#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
fn parse_kib_field(contents: &str, field: &str) -> Option<u64> {
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name == field)
            .then(|| value.split_whitespace().next()?.parse::<u64>().ok())
            .flatten()
    })
}

#[cfg(target_os = "linux")]
fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::parse_kib_field;

    #[test]
    fn parses_exact_smaps_rollup_fields() {
        let sample = "Rss: 120 kB\nPss: 81 kB\nPrivate_Dirty: 37 kB\n";
        assert_eq!(parse_kib_field(sample, "Pss"), Some(81));
        assert_eq!(parse_kib_field(sample, "Private_Dirty"), Some(37));
        assert_eq!(parse_kib_field(sample, "Private_Clean"), None);
    }
}
