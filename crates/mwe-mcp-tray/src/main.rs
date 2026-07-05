// SPDX-License-Identifier: AGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `mwe-mcp-tray` — optional KDE/Linux desktop control surface for the
//! `mwe-mcp` systemd service (roadmap group 14, item 14d).
//!
//! Headless-safe by construction: this is a **separate** binary, so the
//! server never links any of it. The tray is a `StatusNotifierItem` over
//! D-Bus (`ksni` — no GTK, no OpenSSL). It polls and controls the **system**
//! service `mwe-mcp.service` through `systemctl` (start/stop/restart go
//! through polkit), and its icon answers "is it running?" at a glance.
//!
//! Production runs the daemon as the dedicated `mwe-mcp` user under systemd;
//! this tray runs in the desktop user's session and is auto-started with it.
//! On a host with no D-Bus/desktop session the registration fails fast — the
//! tray is simply not used there, the daemon runs headless regardless.

use std::process::Command;
use std::time::Duration;

use ksni::{Tray, TrayMethods};

/// The systemd unit the tray watches and controls.
const SERVICE: &str = "mwe-mcp.service";
/// The production dashboard — the prod instance binds `127.0.0.1:8742`.
const DASHBOARD_URL: &str = "http://127.0.0.1:8742/dashboard";
/// How often the icon is reconciled with the live service state.
const POLL: Duration = Duration::from_secs(3);

/// Tray state — just whether the service is currently active.
#[derive(Debug)]
struct MweTray {
    running: bool,
}

impl Tray for MweTray {
    fn id(&self) -> String {
        "mwe-mcp-tray".into()
    }

    fn icon_name(&self) -> String {
        // The mwe-mcp brand mark (the dashboard favicon), installed into the
        // hicolor icon theme. Coloured when the service runs, a desaturated
        // grey variant when stopped — status at a glance, still on-brand.
        if self.running {
            "mwe-mcp"
        } else {
            "mwe-mcp-inactive"
        }
        .into()
    }

    fn title(&self) -> String {
        "mwe-mcp".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "mwe-mcp".into(),
            description: if self.running {
                "Service running on 127.0.0.1:8742".into()
            } else {
                "Service stopped".into()
            },
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};
        vec![
            // Non-activatable status header — the "is it active?" answer.
            StandardItem {
                label: if self.running {
                    "● running  (:8742)".into()
                } else {
                    "○ stopped".into()
                },
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open dashboard".into(),
                icon_name: "applications-internet".into(),
                activate: Box::new(|_| spawn_detached(&["xdg-open", DASHBOARD_URL])),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open logs".into(),
                icon_name: "utilities-log-viewer".into(),
                activate: Box::new(|_| {
                    spawn_detached(&["konsole", "-e", "journalctl", "-u", SERVICE, "-f"]);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Start".into(),
                icon_name: "media-playback-start".into(),
                enabled: !self.running,
                activate: Box::new(|_| systemctl("start")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Restart".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|_| systemctl("restart")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Stop".into(),
                icon_name: "media-playback-stop".into(),
                enabled: self.running,
                activate: Box::new(|_| systemctl("stop")),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit tray".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Fire-and-forget a helper command — never block the menu thread.
fn spawn_detached(argv: &[&str]) {
    if let [program, args @ ..] = argv {
        let _ = Command::new(program).args(args).spawn();
    }
}

/// `systemctl <action> mwe-mcp.service` — polkit handles the privilege prompt
/// (or a polkit rule grants it silently; see the tray README).
fn systemctl(action: &str) {
    spawn_detached(&["systemctl", action, SERVICE]);
}

/// Whether the systemd unit is currently active.
fn service_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", SERVICE])
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let mut last = service_active();
    let handle = MweTray { running: last }.spawn().await.map_err(|e| {
        anyhow::anyhow!(
            "failed to register the tray StatusNotifierItem (no DBus/desktop session?): {e}"
        )
    })?;
    tracing::info!(service = SERVICE, running = last, "mwe-mcp tray up");

    // Reconcile the icon with the live service state.
    loop {
        tokio::time::sleep(POLL).await;
        let now = service_active();
        if now != last {
            last = now;
            handle.update(move |t: &mut MweTray| t.running = now).await;
        }
    }
}
