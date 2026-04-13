use libproc::libproc::task_info::TaskInfo;
use libproc::proc_pid::pidinfo;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug)]
pub struct ServiceContext {
    pub pid: i32,
    pub name: String,
    pub command: String,
    pub ports: Vec<u16>,
    pub started_at: SystemTime,
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
    /// 0.0 - 100.0 per logical core; derived from deltas between samples.
    pub cpu_pct: f32,
    last_sample: Option<CpuSample>,
}

#[derive(Debug)]
struct CpuSample {
    at: Instant,
    cpu_nsecs: u64,
}

impl ServiceContext {
    pub fn new(
        pid: i32,
        name: String,
        command: String,
        ports: Vec<u16>,
        started_at: SystemTime,
    ) -> Self {
        let mut ctx = Self {
            pid,
            name,
            command,
            ports,
            started_at,
            rss_bytes: 0,
            virtual_bytes: 0,
            cpu_pct: 0.0,
            last_sample: None,
        };
        ctx.sample_resources();
        ctx
    }

    /// Refresh memory + CPU numbers via libproc. Cheap — safe to call every tick.
    pub fn sample_resources(&mut self) {
        let Ok(info) = pidinfo::<TaskInfo>(self.pid, 0) else {
            return;
        };
        self.rss_bytes = info.pti_resident_size;
        self.virtual_bytes = info.pti_virtual_size;

        let cpu_nsecs = info.pti_total_user.saturating_add(info.pti_total_system);
        let now = Instant::now();
        if let Some(prev) = &self.last_sample {
            let wall_ns = now.duration_since(prev.at).as_nanos() as u64;
            if wall_ns > 0 {
                let delta = cpu_nsecs.saturating_sub(prev.cpu_nsecs);
                let pct = (delta as f64 / wall_ns as f64) * 100.0;
                self.cpu_pct = pct.clamp(0.0, 9999.0) as f32;
            }
        }
        self.last_sample = Some(CpuSample { at: now, cpu_nsecs });
    }

    pub fn uptime(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.started_at)
            .unwrap_or_default()
    }
}

/// Scan the given process group for the first pid that has listening TCP
/// sockets, via `lsof -g <pgid>`. Returns (pid, cmd-from-lsof, ports).
pub fn detect_service(pgid: i32) -> Option<(i32, String, Vec<u16>)> {
    // The `-a` flag is load-bearing: without it lsof OR-combines -iTCP and
    // -g <pgid>, which returns every listening TCP process on the machine
    // (e.g. macOS ControlCenter on :5000/:7000). With -a, all filters AND.
    let output = Command::new("/usr/sbin/lsof")
        .args([
            "-a",
            "-iTCP",
            "-sTCP:LISTEN",
            "-g",
            &pgid.to_string(),
            "-n",
            "-P",
            "-Fpcn",
        ])
        .output()
        .ok()?;
    // lsof returns non-zero when there are no matches — that's fine.
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut current_pid: Option<i32> = None;
    let mut current_cmd: Option<String> = None;
    let mut current_ports: Vec<u16> = Vec::new();
    let mut result: Option<(i32, String, Vec<u16>)> = None;

    let flush = |pid: &mut Option<i32>,
                 cmd: &mut Option<String>,
                 ports: &mut Vec<u16>,
                 out: &mut Option<(i32, String, Vec<u16>)>| {
        if out.is_none() {
            if let (Some(p), Some(c)) = (pid.take(), cmd.take()) {
                if !ports.is_empty() {
                    *out = Some((p, c, std::mem::take(ports)));
                    return;
                }
            }
        }
        *pid = None;
        *cmd = None;
        ports.clear();
    };

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let mut chars = line.chars();
        let Some(prefix) = chars.next() else { continue };
        let rest: String = chars.collect();
        match prefix {
            'p' => {
                flush(
                    &mut current_pid,
                    &mut current_cmd,
                    &mut current_ports,
                    &mut result,
                );
                current_pid = rest.parse().ok();
            }
            'c' => {
                current_cmd = Some(rest);
            }
            'n' => {
                // Examples: "*:3000", "127.0.0.1:5432", "[::1]:8080"
                if let Some(port_str) = rest.rsplit(':').next() {
                    if let Ok(port) = port_str.parse::<u16>() {
                        if !current_ports.contains(&port) {
                            current_ports.push(port);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    flush(
        &mut current_pid,
        &mut current_cmd,
        &mut current_ports,
        &mut result,
    );
    result
}

pub fn format_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 {
        format!("{:.1} {}", v, UNITS[u])
    } else {
        format!("{:.0} {}", v, UNITS[u])
    }
}
