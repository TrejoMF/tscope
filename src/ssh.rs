use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::process::ProcessInfo;

/// Flags that consume the next argv token as their argument. Anything not
/// in this set is treated as a standalone flag (or positional host).
const SSH_FLAGS_WITH_ARG: &[&str] = &[
    "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O",
    "-o", "-p", "-Q", "-R", "-S", "-W", "-w",
];

#[derive(Debug)]
pub struct SshContext {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    /// Anything after the host on the ssh command line — e.g. `ssh foo@bar "uptime"`.
    pub remote_command: Option<String>,
    /// Process start time (when `ssh` was launched).
    pub started_at: SystemTime,
    /// User-provided alias for this connection (persisted in the config file).
    pub display_name: Option<String>,
    /// Resolved IP (populated asynchronously by a background DNS lookup).
    resolved_ip: Arc<Mutex<Option<String>>>,
}

impl SshContext {
    pub fn try_from_proc(info: &ProcessInfo, config: &crate::config::Config) -> Option<Self> {
        if !info.is_ssh() {
            return None;
        }
        let (user, host, port, remote_command) = parse_ssh_argv(&info.argv)?;
        let started_at = info.start_time.unwrap_or_else(SystemTime::now);
        let display_name = config.lookup_ssh_alias(user.as_deref(), &host);
        let ctx = Self {
            user,
            host,
            port,
            remote_command,
            started_at,
            display_name,
            resolved_ip: Arc::new(Mutex::new(None)),
        };
        ctx.kick_dns_lookup();
        Some(ctx)
    }

    pub fn resolved_ip(&self) -> Option<String> {
        self.resolved_ip.lock().ok()?.clone()
    }

    #[allow(dead_code)] // panel dropped uptime for now; kept for when it returns
    pub fn connection_age(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.started_at)
            .unwrap_or_default()
    }

    fn kick_dns_lookup(&self) {
        // If the host already parses as an IP, skip DNS.
        if self.host.parse::<std::net::IpAddr>().is_ok() {
            *self.resolved_ip.lock().unwrap() = Some(self.host.clone());
            return;
        }
        let host = self.host.clone();
        let port = self.port.unwrap_or(22);
        let slot = Arc::clone(&self.resolved_ip);
        thread::spawn(move || {
            let addr_str = format!("{}:{}", host, port);
            if let Ok(mut iter) = addr_str.to_socket_addrs() {
                if let Some(addr) = iter.next() {
                    if let Ok(mut guard) = slot.lock() {
                        *guard = Some(addr.ip().to_string());
                    }
                }
            }
        });
    }
}

/// Parse an `ssh ...` argv into (user, host, port, trailing remote command).
fn parse_ssh_argv(argv: &[String]) -> Option<(Option<String>, String, Option<u16>, Option<String>)> {
    if argv.is_empty() {
        return None;
    }
    let mut user: Option<String> = None;
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut remote: Vec<String> = Vec::new();

    let mut i = 1; // skip argv[0]
    while i < argv.len() {
        let a = &argv[i];
        if host.is_some() {
            // Anything after the host is the remote command.
            remote.push(a.clone());
            i += 1;
            continue;
        }
        if a == "-l" && i + 1 < argv.len() {
            user = Some(argv[i + 1].clone());
            i += 2;
            continue;
        }
        if a == "-p" && i + 1 < argv.len() {
            port = argv[i + 1].parse().ok();
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            if SSH_FLAGS_WITH_ARG.contains(&a.as_str()) && i + 1 < argv.len() {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // First positional is the host (optionally user@host[:port] but we
        // only honor `user@` + `-p` for port).
        if let Some(at) = a.find('@') {
            user = Some(a[..at].to_string());
            host = Some(a[at + 1..].to_string());
        } else {
            host = Some(a.clone());
        }
        i += 1;
    }

    let host = host?;
    let remote_command = if remote.is_empty() {
        None
    } else {
        Some(remote.join(" "))
    };
    Some((user, host, port, remote_command))
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}
