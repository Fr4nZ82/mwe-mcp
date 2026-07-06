# mwe-mcp-tray

An **optional** desktop control surface for the `mwe-mcp` systemd service —
roadmap group 14,
item 14d. It is a **separate binary**: the server never links any GUI code, so
a headless server runs the daemon unchanged and simply does not run this.

## What it does

A KDE/freedesktop **StatusNotifierItem** (via [`ksni`](https://crates.io/crates/ksni)
— pure D-Bus/`zbus`, no GTK, no OpenSSL). The icon answers *"is it running?"* at
a glance and the menu controls the **system** service `mwe-mcp.service`:

- **Icon** — the **mwe-mcp brand mark** (the dashboard favicon), installed into
  the hicolor icon theme: coloured (`mwe-mcp`) when the service is active, a
  desaturated grey variant (`mwe-mcp-inactive`) when stopped — status at a
  glance, on-brand. Reconciled every 3 s by polling `systemctl is-active`. The
  SVGs ship in `assets/`; install them to
  `~/.local/share/icons/hicolor/scalable/apps/`.
- **Menu** — a status header, **Open dashboard** (`http://127.0.0.1:8742/dashboard`),
  **Open logs** (`journalctl -u mwe-mcp.service -f` in Konsole), **Start** /
  **Restart** / **Stop**, and **Quit tray**.

Service control goes through `systemctl` → **polkit**. Without a rule, polkit
shows a password dialog; with the rule below it is silent.

## Build & install

```bash
cargo build --release -p mwe-mcp-tray
sudo install -m755 target/release/mwe-mcp-tray /usr/local/bin/mwe-mcp-tray
```

## Wiring (Linux/KDE)

**Polkit** — let the desktop user manage the unit without a password
(`/etc/polkit-1/rules.d/49-mwe-mcp.rules`, replace the user):

```javascript
polkit.addRule(function(action, subject) {
    if (action.id == "org.freedesktop.systemd1.manage-units" &&
        action.lookup("unit") == "mwe-mcp.service" &&
        subject.user == "frodo") {
        return polkit.Result.YES;
    }
});
```

**Auto-start with the desktop session** (`~/.config/autostart/mwe-mcp-tray.desktop`)
and an optional **menu entry** (`~/.local/share/applications/mwe-mcp.desktop`)
both just `Exec=/usr/local/bin/mwe-mcp-tray`. On a headless host there is no
graphical session, so the autostart never runs — the tray is absent and the
daemon runs headless regardless.

## Scope

Linux/KDE only for now; the macOS/Windows form is roadmap 14e (required for
v1.0). The tray targets the production instance on `127.0.0.1:8742`.
