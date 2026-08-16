//! Cross-platform process CPU accounting for resource benchmarks.

use std::time::Duration;

pub(crate) fn process_cpu_time(pids: &[u32]) -> Duration {
    #[cfg(target_os = "linux")]
    {
        let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        assert!(ticks_per_second > 0, "sysconf(_SC_CLK_TCK) failed");
        let ticks: u64 = pids.iter().map(|pid| proc_cpu_ticks(*pid)).sum();
        return Duration::from_secs_f64(ticks as f64 / ticks_per_second as f64);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let pid_list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = std::process::Command::new("ps")
            .args(["-o", "time=", "-p", &pid_list])
            .output()
            .expect("sample process CPU time");
        assert!(output.status.success(), "ps failed while sampling CPU time");
        let centiseconds: u64 = String::from_utf8(output.stdout)
            .expect("ps CPU output is UTF-8")
            .lines()
            .map(parse_cpu_centiseconds)
            .sum();
        Duration::from_millis(centiseconds * 10)
    }
}

#[cfg(target_os = "linux")]
fn proc_cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .unwrap_or_else(|error| panic!("read CPU accounting for process {pid}: {error}"));
    // The second field is parenthesized and may contain spaces or ')'. Split
    // after its final ')' so indices below follow proc_pid_stat(5): token 0 is
    // field 3 (state), token 11 is utime, and token 12 is stime.
    let fields = stat[stat
        .rfind(')')
        .unwrap_or_else(|| panic!("invalid /proc/{pid}/stat"))
        + 1..]
        .split_whitespace()
        .collect::<Vec<_>>();
    assert!(fields.len() > 12, "truncated /proc/{pid}/stat");
    fields[11]
        .parse::<u64>()
        .expect("parse process user CPU ticks")
        + fields[12]
            .parse::<u64>()
            .expect("parse process system CPU ticks")
}

#[cfg(not(target_os = "linux"))]
fn parse_cpu_centiseconds(value: &str) -> u64 {
    let parts: Vec<_> = value.trim().split(':').collect();
    assert!(
        (2..=3).contains(&parts.len()),
        "unexpected ps time: {value:?}"
    );
    let seconds = parts[parts.len() - 1];
    let (seconds, centiseconds) = seconds.split_once('.').unwrap_or((seconds, "0"));
    let minutes = parts[parts.len() - 2]
        .parse::<u64>()
        .expect("parse CPU minutes");
    let hours = if parts.len() == 3 {
        parts[0].parse::<u64>().expect("parse CPU hours")
    } else {
        0
    };
    (hours * 3_600 + minutes * 60 + seconds.parse::<u64>().expect("parse CPU seconds")) * 100
        + centiseconds.parse::<u64>().expect("parse CPU centiseconds")
}
