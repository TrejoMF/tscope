use std::process::Command;
use std::time::SystemTime;

use crate::process::ProcessInfo;

/// Context attached to a pane whose foreground process is a `docker` command.
#[derive(Debug)]
pub struct DockerContext {
    pub subcommand: String,
    pub container: Option<String>,
    pub image: Option<String>,
    pub started_at: SystemTime,
    pub extra_args: Vec<String>,
}

impl DockerContext {
    pub fn try_from_proc(info: &ProcessInfo) -> Option<Self> {
        if !info.is_docker() {
            return None;
        }
        let (subcommand, container, image, extra_args) = parse_docker_argv(&info.argv)?;
        let started_at = info.start_time.unwrap_or_else(SystemTime::now);
        Some(Self {
            subcommand,
            container,
            image,
            started_at,
            extra_args,
        })
    }

    #[allow(dead_code)] // panel dropped uptime for now; kept for when it returns
    pub fn uptime(&self) -> std::time::Duration {
        SystemTime::now()
            .duration_since(self.started_at)
            .unwrap_or_default()
    }
}

/// Parse `docker ...` argv into (subcommand, container, image, extra_args).
fn parse_docker_argv(
    argv: &[String],
) -> Option<(String, Option<String>, Option<String>, Vec<String>)> {
    if argv.is_empty() {
        return None;
    }

    let mut i = 1; // skip argv[0]

    // Skip global docker flags (e.g. -H, --host, --context, --log-level).
    while i < argv.len() && argv[i].starts_with('-') {
        let flag = &argv[i];
        i += 1;
        if matches!(
            flag.as_str(),
            "-H" | "--host" | "-c" | "--context" | "-l" | "--log-level"
        ) && i < argv.len()
        {
            i += 1;
        }
    }

    let subcommand = argv.get(i)?.clone();
    i += 1;

    // Handle `docker compose <sub>` as a compound subcommand.
    if subcommand == "compose" {
        while i < argv.len() && argv[i].starts_with('-') {
            i += 1;
        }
        let compose_sub = argv.get(i).map(|s| s.as_str()).unwrap_or("up");
        let full_sub = format!("compose {}", compose_sub);
        i += 1;
        let extra: Vec<String> = argv.get(i..).unwrap_or_default().to_vec();
        return Some((full_sub, None, None, extra));
    }

    let mut container: Option<String> = None;
    let mut image: Option<String> = None;
    let mut extra = Vec::new();

    // Walk past flags to find the first positional (container or image name).
    while i < argv.len() {
        let a = &argv[i];
        if a.starts_with('-') {
            extra.push(a.clone());
            i += 1;
            // Consume flag value if it looks like one.
            if i < argv.len() && !argv[i].starts_with('-') {
                extra.push(argv[i].clone());
                i += 1;
            }
        } else {
            match subcommand.as_str() {
                "run" | "create" | "pull" | "push" | "build" => {
                    image = Some(a.clone());
                }
                _ => {
                    container = Some(a.clone());
                }
            }
            i += 1;
            extra.extend_from_slice(argv.get(i..).unwrap_or_default());
            break;
        }
    }

    Some((subcommand, container, image, extra))
}

/// A single container as returned by `docker ps`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DockerContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub ports: String,
    pub state: String,
}

/// List containers via `docker ps -a --format json`. Returns an empty vec if
/// Docker is not installed or the daemon isn't reachable.
pub fn list_containers() -> Vec<DockerContainer> {
    let output = match Command::new("docker")
        .args(["ps", "-a", "--format", "{{json .}}"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    if !output.status.success() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut containers = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        containers.push(DockerContainer {
            id: json_str(&val, "ID"),
            name: json_str(&val, "Names"),
            image: json_str(&val, "Image"),
            status: json_str(&val, "Status"),
            ports: json_str(&val, "Ports"),
            state: json_str(&val, "State"),
        });
    }

    containers
}

fn json_str(val: &serde_json::Value, key: &str) -> String {
    val.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}
