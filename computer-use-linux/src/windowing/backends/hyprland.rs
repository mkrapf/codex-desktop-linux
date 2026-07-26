use crate::terminal::enrich_terminal_windows;
use crate::windowing::registry::BackendProbe;
use crate::windowing::types::{WindowBounds, WindowInfo};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

pub const HYPRLAND_BACKEND: &str = "hyprland";

pub fn probe() -> BackendProbe {
    match hyprctl_output(&["clients", "-j"]) {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let ok = matches!(
                serde_json::from_str::<serde_json::Value>(&stdout),
                Ok(serde_json::Value::Array(_))
            );
            BackendProbe {
                id: HYPRLAND_BACKEND,
                ok,
                can_list_windows: ok,
                can_focus_apps: ok,
                can_focus_windows: ok,
                detail: if ok {
                    format!(
                        "hyprctl clients -j returned a JSON array; {}",
                        hyprland_config_provider().focus_detail()
                    )
                } else {
                    "hyprctl clients -j did not return a JSON array".to_string()
                },
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            BackendProbe {
                id: HYPRLAND_BACKEND,
                ok: false,
                can_list_windows: false,
                can_focus_apps: false,
                can_focus_windows: false,
                detail: if stderr.is_empty() { stdout } else { stderr },
            }
        }
        Err(error) => BackendProbe {
            id: HYPRLAND_BACKEND,
            ok: false,
            can_list_windows: false,
            can_focus_apps: false,
            can_focus_windows: false,
            detail: error.to_string(),
        },
    }
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let output = hyprctl_output(&["clients", "-j"]).context("failed to run hyprctl clients -j")?;
    if !output.status.success() {
        bail!(
            "hyprctl clients -j failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse_hyprland_clients(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn parse_hyprland_clients(json: &str) -> Result<Vec<WindowInfo>> {
    let clients: Vec<HyprlandClient> =
        serde_json::from_str(json).context("failed to parse hyprctl clients -j output")?;

    let mut windows = clients
        .into_iter()
        .filter(|client| client.mapped.unwrap_or(true))
        .map(WindowInfo::try_from)
        .collect::<Result<Vec<_>>>()?;
    windows.sort_by_key(|window| window.window_id);
    enrich_terminal_windows(&mut windows);
    Ok(windows)
}

pub fn activate_window(window_id: u64) -> Result<()> {
    let address = format!("address:0x{window_id:x}");
    let provider = hyprland_config_provider();
    let mut failures = Vec::new();

    for args in focus_dispatch_attempts(provider, &address) {
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let command_label = format!("hyprctl {}", args.join(" "));
        let output =
            hyprctl_output(&arg_refs).with_context(|| format!("failed to run {command_label}"))?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        failures.push(format!("{command_label} failed: {detail}"));
    }

    bail!("Hyprland focus dispatch failed: {}", failures.join("; "))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HyprlandConfigProvider {
    Lua,
    Legacy,
    Unknown,
}

impl HyprlandConfigProvider {
    fn focus_detail(self) -> &'static str {
        match self {
            Self::Lua => "Lua config provider detected; focus uses hl.dsp.focus",
            Self::Legacy => "legacy config provider detected; focus uses focuswindow",
            Self::Unknown => {
                "config provider was not reported; focus tries focuswindow then hl.dsp.focus"
            }
        }
    }
}

fn hyprland_config_provider() -> HyprlandConfigProvider {
    let Ok(output) = hyprctl_output(&["status"]) else {
        return HyprlandConfigProvider::Unknown;
    };
    if !output.status.success() {
        return HyprlandConfigProvider::Unknown;
    }
    parse_config_provider(&String::from_utf8_lossy(&output.stdout))
}

fn parse_config_provider(status: &str) -> HyprlandConfigProvider {
    let Some(provider) = status.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case("configProvider")
            .then(|| value.trim())
    }) else {
        return HyprlandConfigProvider::Unknown;
    };

    if provider.eq_ignore_ascii_case("lua") {
        HyprlandConfigProvider::Lua
    } else {
        HyprlandConfigProvider::Legacy
    }
}

fn focus_dispatch_attempts(provider: HyprlandConfigProvider, address: &str) -> Vec<Vec<String>> {
    let legacy = vec![
        "dispatch".to_string(),
        "focuswindow".to_string(),
        address.to_string(),
    ];
    let lua = vec![
        "dispatch".to_string(),
        format!("hl.dsp.focus({{ window = '{address}' }})"),
    ];

    match provider {
        HyprlandConfigProvider::Lua => vec![lua],
        HyprlandConfigProvider::Legacy => vec![legacy],
        HyprlandConfigProvider::Unknown => vec![legacy, lua],
    }
}

fn hyprctl_output(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut command = Command::new("hyprctl");
    let has_signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    if !has_signature {
        if let Some(signature) = infer_hyprland_instance_signature() {
            command.args(["-i", &signature]);
        }
    }
    command.args(args).output()
}

fn infer_hyprland_instance_signature() -> Option<String> {
    let runtime = xdg_runtime_dir()?;
    let hypr_dir = runtime.join("hypr");
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let candidates = fs::read_dir(hypr_dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            let signature = path.file_name()?.to_string_lossy().into_owned();
            hyprland_instance_candidate(&path, signature, wayland_display.as_deref())
        })
        .collect::<Vec<_>>();

    select_hyprland_instance(candidates).map(|candidate| candidate.signature)
}

fn hyprland_instance_candidate(
    path: &Path,
    signature: String,
    wayland_display: Option<&str>,
) -> Option<HyprlandInstanceCandidate> {
    if !path
        .join(".socket.sock")
        .metadata()
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
    {
        return None;
    }

    let lock = fs::read_to_string(path.join("hyprland.lock")).ok()?;
    let mut lines = lock.lines();
    let pid = lines.next()?.trim();
    if pid.is_empty() || !Path::new("/proc").join(pid).exists() {
        return None;
    }
    let lock_wayland_display = lines
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let wayland_display_matches =
        wayland_display.is_some() && lock_wayland_display == wayland_display;
    let modified = path
        .join(".socket.sock")
        .metadata()
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    Some(HyprlandInstanceCandidate {
        signature,
        wayland_display_matches,
        modified,
    })
}

fn select_hyprland_instance(
    candidates: Vec<HyprlandInstanceCandidate>,
) -> Option<HyprlandInstanceCandidate> {
    candidates
        .into_iter()
        .max_by_key(|candidate| (candidate.wayland_display_matches, candidate.modified))
}

fn xdg_runtime_dir() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(value));
    }
    let uid = fs::metadata("/proc/self").ok()?.uid();
    Some(PathBuf::from(format!("/run/user/{uid}")))
}

#[derive(Debug)]
struct HyprlandInstanceCandidate {
    signature: String,
    wayland_display_matches: bool,
    modified: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_lua_config_provider_from_hyprctl_status() {
        let status = "Hyprland 0.56.0\nconfigProvider: lua\n";

        assert_eq!(parse_config_provider(status), HyprlandConfigProvider::Lua);
    }

    #[test]
    fn treats_non_lua_config_provider_as_legacy() {
        let status = "Hyprland 0.55.0\nconfigProvider: hyprlang\n";

        assert_eq!(
            parse_config_provider(status),
            HyprlandConfigProvider::Legacy
        );
    }

    #[test]
    fn builds_lua_focus_dispatch_for_lua_config_provider() {
        let attempts = focus_dispatch_attempts(HyprlandConfigProvider::Lua, "address:0x1234abcd");

        assert_eq!(
            attempts,
            vec![vec![
                "dispatch".to_string(),
                "hl.dsp.focus({ window = 'address:0x1234abcd' })".to_string(),
            ]]
        );
    }

    #[test]
    fn falls_back_to_lua_focus_when_config_provider_is_unknown() {
        let attempts =
            focus_dispatch_attempts(HyprlandConfigProvider::Unknown, "address:0x1234abcd");

        assert_eq!(attempts.len(), 2);
        assert_eq!(
            attempts[0],
            vec![
                "dispatch".to_string(),
                "focuswindow".to_string(),
                "address:0x1234abcd".to_string(),
            ]
        );
        assert_eq!(
            attempts[1],
            vec![
                "dispatch".to_string(),
                "hl.dsp.focus({ window = 'address:0x1234abcd' })".to_string(),
            ]
        );
    }

    #[test]
    fn selects_wayland_matching_hyprland_instance_before_newer_nonmatch() {
        let older_match = HyprlandInstanceCandidate {
            signature: "match".to_string(),
            wayland_display_matches: true,
            modified: SystemTime::UNIX_EPOCH,
        };
        let newer_nonmatch = HyprlandInstanceCandidate {
            signature: "nonmatch".to_string(),
            wayland_display_matches: false,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        };

        let selected = select_hyprland_instance(vec![older_match, newer_nonmatch]).unwrap();

        assert_eq!(selected.signature, "match");
    }

    #[test]
    fn selects_newest_hyprland_instance_when_wayland_match_is_tied() {
        let older = HyprlandInstanceCandidate {
            signature: "older".to_string(),
            wayland_display_matches: false,
            modified: SystemTime::UNIX_EPOCH,
        };
        let newer = HyprlandInstanceCandidate {
            signature: "newer".to_string(),
            wayland_display_matches: false,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        };

        let selected = select_hyprland_instance(vec![older, newer]).unwrap();

        assert_eq!(selected.signature, "newer");
    }
}

#[derive(Debug, Deserialize)]
struct HyprlandClient {
    address: String,
    mapped: Option<bool>,
    hidden: Option<bool>,
    at: Option<[i32; 2]>,
    size: Option<[u32; 2]>,
    workspace: Option<HyprlandWorkspace>,
    #[serde(rename = "class")]
    class_name: Option<String>,
    title: Option<String>,
    pid: Option<i64>,
    xwayland: Option<bool>,
    #[serde(rename = "focusHistoryID")]
    focus_history_id: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct HyprlandWorkspace {
    id: Option<i32>,
}

impl TryFrom<HyprlandClient> for WindowInfo {
    type Error = anyhow::Error;

    fn try_from(client: HyprlandClient) -> Result<Self> {
        let window_id = parse_hyprland_address(&client.address)?;
        let bounds = client.size.map(|[width, height]| WindowBounds {
            x: client.at.map(|[x, _]| x),
            y: client.at.map(|[_, y]| y),
            width,
            height,
        });
        let client_type = client.xwayland.map(|xwayland| {
            if xwayland {
                "x11".to_string()
            } else {
                "wayland".to_string()
            }
        });

        Ok(WindowInfo {
            window_id,
            title: client.title,
            app_id: client.class_name.clone(),
            wm_class: client.class_name,
            pid: client.pid.and_then(|pid| u32::try_from(pid).ok()),
            bounds,
            workspace: client.workspace.and_then(|workspace| workspace.id),
            focused: client.focus_history_id == Some(0),
            hidden: client.hidden.unwrap_or(false),
            client_type,
            backend: HYPRLAND_BACKEND.to_string(),
            terminal: None,
        })
    }
}

fn parse_hyprland_address(address: &str) -> Result<u64> {
    let hex = address
        .trim()
        .strip_prefix("0x")
        .context("Hyprland window address did not start with 0x")?;
    u64::from_str_radix(hex, 16)
        .with_context(|| format!("failed to parse Hyprland window address {address}"))
}
