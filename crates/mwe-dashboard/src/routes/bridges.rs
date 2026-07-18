// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bridge **onboarding + distribution** surface.
//!
//! The same non-secret content is reachable two ways, so we never have
//! to ask "who is visiting":
//!
//! - **Public** (root-mounted, anonymous) for agents and `curl`:
//!   - `GET /` — slim product front page: a line pointing an agent at
//!     the bridge catalog, and a sign-in link for the human.
//!   - `GET /bridges` + `GET /bridges/:consumer` — the catalog and the
//!     per-bridge install guide. Every entry carries an **"agent
//!     instructions"** link straight to its machine-readable `install.md`,
//!     so an agent doesn't need the human guide at all.
//!   - `GET /bridges/:consumer/install.{sh,ps1,md}` — the self-contained
//!     installers (plugin tree embedded via [`rust_embed`] and inlined as
//!     heredocs / here-strings; one `curl … | sh`, no `tar`/`jq`/bundle).
//! - **Dashboard tab** (`/dashboard/bridges`, authenticated) for the
//!   operator: the *same* catalog + guide bodies wrapped in the dashboard
//!   shell, so "Bridges" sits in the nav next to Wikis / Facts / Settings.
//!   Shared body functions take a base prefix so the in-page links resolve
//!   under `/dashboard` there and at the root publicly.
//!
//! The **token never lives here** — it is a credential, issued from the
//! dashboard home's "Connect a consumer" card. These pages and scripts
//! only ever instruct the operator to mint it, disable the host's
//! built-in memory, and restart.

use axum::Router;
use axum::extract::{Host, Path};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use maud::{Markup, html};
use rust_embed::RustEmbed;

use crate::auth::SessionUser;
use crate::state::DashboardState;
use crate::ui::layout;

/// The hermes bridge plugin tree, embedded from the in-repo bridge
/// directory. Python build artifacts (`__pycache__`/`*.pyc`) are
/// filtered out in [`plugin_files`] rather than via rust-embed's
/// `#[exclude]` (which would pull in the `include-exclude` feature and
/// its glob dependencies for no real gain), so they never reach a
/// consumer's checkout.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../agents-bridges/hermes/plugins/"]
struct HermesBridge;

/// Catalog of bridged consumers, in display order. A consumer appears
/// here only when its bridge ships a served onboarding surface — a
/// `curl … | sh` plugin installer (hermes) **or** an agent-driven
/// `install.md` (claude-code).
const BRIDGES: &[(&str, &str)] = &[
    ("hermes", "Hermes (Nous Research)"),
    ("claude-code", "Claude Code (Anthropic)"),
];

/// Heredoc / here-string delimiter for the inlined plugin files. Chosen
/// so it cannot appear in Python/YAML source; asserted by a test.
const SH_DELIM: &str = "MWE_BRIDGE_EOF";

fn bridge_label(consumer: &str) -> Option<&'static str> {
    BRIDGES
        .iter()
        .find(|(name, _)| *name == consumer)
        .map(|(_, label)| *label)
}

/// Public, anonymous bridge-distribution router, mounted at the **root**
/// of the HTTP tree by `mwe-mcp-server` (next to `/cite`). Stateless.
pub fn public_site_router() -> Router {
    Router::new()
        .route("/", get(front_page))
        .route("/bridges", get(public_bridges_index))
        .route("/bridges/:consumer", get(public_bridge_page))
        .route("/bridges/:consumer/install.sh", get(install_sh))
        .route("/bridges/:consumer/install.ps1", get(install_ps1))
        .route("/bridges/:consumer/install.md", get(install_md))
}

/// Authenticated "Bridges" tab, merged into the dashboard tree under
/// `/dashboard`. Same catalog + guide bodies as the public surface, but
/// wrapped in the dashboard shell (top nav) so it reads as a tab.
pub fn dashboard_tab_router() -> Router<DashboardState> {
    Router::new()
        .route("/bridges", get(tab_bridges_index))
        .route("/bridges/:consumer", get(tab_bridge_page))
}

// ---------------------------------------------------------------------
// Origin
// ---------------------------------------------------------------------

/// Derive the public origin (`scheme://host`) from the request `Host`
/// header — the heuristic the onboarding pages share so the command
/// shown reflects however the operator reached us. `http` for loopback,
/// `https` otherwise.
fn origin_from_host(host: &str) -> String {
    let scheme = if host.starts_with("localhost") || host.starts_with("127.") {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{host}")
}

// ---------------------------------------------------------------------
// Shared page bodies (rendered under both the public root and /dashboard)
// ---------------------------------------------------------------------

/// Front page body. `base` prefixes the in-app catalog link.
fn front_body(base: &str) -> Markup {
    html! {
        p.muted {
            "Agent-agnostic, governed, persistent memory over MCP. Your memory "
            "stays a folder you control; consumers connect over HTTP."
        }
        h2 { "If you are an agent" }
        p {
            "Read the setup instructions for connecting yourself to this memory: "
            a href=(format!("{base}/bridges")) { (format!("{base}/bridges")) }
            " — pick your host and follow its " code { "install.md" } "."
        }
        h2 { "If you are a human" }
        p {
            a href="/dashboard/" { "Sign in" }
            " to set up and manage this memory (first run walks you through "
            "claiming the admin)."
        }
    }
}

/// Catalog body. `base` prefixes the per-consumer guide link; the
/// "agent instructions" link always points at the public `install.md`.
/// `origin` (`scheme://host`) is shown in the bridge-less claude.ai section.
fn catalog_body(base: &str, origin: &str) -> Markup {
    html! {
        p.muted {
            "Bridges connect a host agent to mwe-mcp. A "
            strong { "standard" } " consumer (hermes) is wired at full fidelity "
            "— one ingest per turn, recall block, per-sender attribution. A "
            strong { "smart" } " consumer (Claude Code) brings its own LLM and "
            "authors a project's companion-wiki over MCP. Pick your host for the "
            "setup, or hand the agent its " code { "install.md" } " directly."
        }
        table {
            thead { tr {
                th { "Consumer" }
                th { "Setup" }
                th { "For agents" }
            } }
            tbody {
                @for (name, label) in BRIDGES {
                    tr {
                        td { strong { (label) } " · " code { (name) } }
                        td {
                            a href=(format!("{base}/bridges/{name}")) {
                                "Set up the " (name) " bridge →"
                            }
                        }
                        td {
                            a href=(format!("/bridges/{name}/install.md")) {
                                "agent instructions"
                            }
                        }
                    }
                }
            }
        }

        (claude_ai_section(origin))
    }
}

/// Instructions for connecting the **claude.ai web app** — not a bridge: it has
/// no local install, it connects as a *smart* consumer over the `webagentoauth`
/// OAuth flow (no token to copy). Authors its own dedicated wiki; recalls and
/// saves on request. The exact field names in claude.ai's UI may differ — this
/// is the manual path until it is verified live against the public endpoint.
fn claude_ai_section(origin: &str) -> Markup {
    html! {
        h2 { "Connect the claude.ai web app" }
        p.muted {
            "claude.ai (Pro / Max / Team) connects directly as a smart memory agent "
            "over OAuth — no bridge to install, no token to copy. It authors its own "
            "dedicated wiki, and searches or saves when you ask it to."
        }
        ol {
            li {
                "In claude.ai, open " strong { "Settings → Connectors" }
                " and choose " strong { "Add custom connector" } "."
            }
            li {
                "Paste this server's MCP URL: "
                code { (format!("{origin}/mcp")) }
            }
            li {
                "claude.ai redirects you here to sign in and approve the connection "
                "(the " code { "webagentoauth" } " flow). Approve it — a dedicated "
                "wiki is created for it."
            }
            li {
                "For the best behavior, teach claude.ai how to use this memory: "
                a href="/webagentoauth/skill.md" download="mwe-mcp-memory.md" {
                    "download the mwe-mcp skill"
                }
                " and add it in claude.ai → " strong { "Settings → Capabilities → Upload skill" }
                ". (Set the memory tools to auto-allow so it doesn't ask every time.)"
            }
        }
        p.muted {
            "Approved connections — and a Disconnect button — live on the "
            a href="/dashboard/tokens" { "Tokens" } " page."
        }
    }
}

/// Per-consumer install guide body. No token here — that lives on the
/// dashboard home's "Connect a consumer" card. Dispatches on the
/// consumer: hermes ships a `curl … | sh` plugin installer; claude-code
/// is an agent-driven `install.md` (no plugins, no shell installer).
fn guide_body(consumer: &str, origin: &str) -> Markup {
    match consumer {
        "claude-code" => claude_code_guide_body(origin),
        _ => hermes_guide_body(consumer, origin),
    }
}

/// Human guide for the **Claude Code** smart-consumer bridge: register the
/// MCP server and sign in over OAuth (no token), then install the
/// strongly-recommended session-start hook. No plugins and no `curl … | sh`. The
/// agent registers the server itself; the OAuth sign-in and the hook are the
/// operator's (the agent stops and asks) — see `install.md`.
fn claude_code_guide_body(origin: &str) -> Markup {
    let mcp_add = format!("claude mcp add --transport http mwe-mcp {origin}/mcp --scope user");
    let agent_line = format!(
        "Read {origin}/bridges/claude-code/install.md and follow the instructions to connect me to this memory."
    );
    html! {
        p.muted {
            "Claude Code connects as a " strong { "smart consumer" } ": it brings "
            "its own subscription LLM and is a native MCP client, so there are no "
            "plugins to install. It signs in over " strong { "OAuth — no token to "
            "copy" } " — and gets its own operational-memory wiki plus per-project "
            "memory it authors as you work."
        }

        h2 { "Connect (OAuth — no token)" }
        ol {
            li {
                "Register this server at user scope:"
                pre.endpoint-display { (mcp_add) }
                "(user scope so it resolves in every session, including the "
                "session-start hook below.)"
            }
            li {
                "Run " code { "/mcp" } " in a Claude Code session (or "
                code { "claude mcp login mwe-mcp" } "). Claude Code opens your "
                "browser to sign in here and approve the connection — the "
                code { "webagentoauth" } " flow. " strong { "No token is pasted." }
            }
            li {
                "On approve, a dedicated " strong { "operational wiki" } " is forged "
                "for the connection (Claude Code keeps its general working memory, "
                "its behaviour rules and a conversation log there). Project knowledge "
                "goes to per-project wikis; facts about you go to your personal memory."
            }
        }
        p.muted {
            "If you connected mid-session, " strong { "reload Claude Code" }
            " (or open a fresh session) so the mwe-mcp tools become available — "
            "\"Connected\" alone doesn't load them into the running session."
        }

        h2 { "Let the agent do the setup parts" }
        p { "Or paste this to a running Claude Code session — it registers the server "
            "and walks you through the rest (the OAuth sign-in and the hook are yours "
            "to approve / add):" }
        pre.endpoint-display { (agent_line) }
        p.muted {
            "Machine-readable form: "
            a href="/bridges/claude-code/install.md" { "/bridges/claude-code/install.md" }
        }

        h2 { "Session-start recall hook — strongly recommended" }
        p.muted {
            "Without this hook, Claude Code may " strong { "not auto-recall or "
            "auto-capture at all" } ": the MCP server's nudge is passive, and like "
            "claude.ai the model tends to stay idle until asked. This hook is what makes "
            "recall + capture fire at the start of " strong { "every" } " session. Add the "
            code { "SessionStart" } " hook from "
            code { "/connect/hooks/claude-code.json" } " to "
            code { "~/.claude/settings.json" } ": a token-less command hook that injects "
            "a fixed reminder to call " code { "smart_bootstrap" } " + recall (the recall "
            "itself stays the model's own tool call). " strong { "You add it yourself" }
            " — an agent can't merge an external hook into its settings (Claude Code "
            "blocks that as self-modification), unless it is running in "
            code { "bypass-permissions" } " mode, where it can add it for you."
        }

        h3 { "Keeping a repo private" }
        p.muted {
            "Memory is active in every Claude Code session on the machine. To opt one "
            "project out entirely — no recall, no save, nothing leaves it — add a "
            "per-project override in that repo's "
            code { ".claude/settings.json" } ": "
            code { "{\"mcpServers\": {\"mwe-mcp\": null}}" } ". A work repo on a "
            "separate memory server instead points its own "
            code { ".mcp.json" } " / settings at that server's origin. See "
            code { "INTEGRATING.md" } " (\"Per-project isolation\") for the full topology."
        }
    }
}

/// Human guide for the **hermes** standard-consumer bridge — the
/// `curl … | sh` plugin installer. No token here — that lives on the
/// dashboard home's "Connect a consumer" card.
fn hermes_guide_body(consumer: &str, origin: &str) -> Markup {
    let curl = format!("curl -fsSL {origin}/bridges/{consumer}/install.sh | sh");
    let ps = format!("irm {origin}/bridges/{consumer}/install.ps1 | iex");
    let agent_line = format!(
        "Read {origin}/bridges/{consumer}/install.md and follow the instructions to connect me to this memory."
    );
    html! {
        p.muted {
            "First-party, served by this server. Follow it by hand below, or "
            "hand it to the agent."
        }

        h2 { "1. Install the plugins" }
        p { "Run this " strong { "from inside your hermes-agent checkout" }
            " (the context-engine plugin must land there). Linux / macOS:" }
        pre.endpoint-display { (curl) }
        p.muted { "Windows (PowerShell): " code { (ps) } }
        p.muted {
            "Where the files land: the memory + media plugins go to "
            code { "~/.hermes/plugins/" } ", and the context-engine plugin goes "
            "into the hermes-agent checkout you run the command from. You normally "
            "don't set anything — but if your layout differs, two environment "
            "variables override the defaults: " code { "HERMES_HOME" }
            " (hermes's runtime dir, default " code { "~/.hermes" } ") and "
            code { "HERMES_SRC" }
            " (your hermes-agent checkout, if you don't run the installer from inside it)."
        }

        h3 { "…or let the agent do it" }
        p { "Paste this to a running hermes — it runs the same installer itself:" }
        pre.endpoint-display { (agent_line) }
        p.muted {
            "Machine-readable form: "
            a href=(format!("/bridges/{consumer}/install.md")) {
                "/bridges/" (consumer) "/install.md"
            }
        }

        h2 { "2. Finish (the steps the installer leaves to you)" }
        p {
            "The installer never touches your token. After the files are in place: "
        }
        ul {
            li { "Issue a token from the " a href="/dashboard/home" { "dashboard home" }
                " and set it as " code { "MWE_TOKEN" } " in hermes's " code { ".env" } "." }
            li { "Set " code { "memory_enabled: false" } " and "
                code { "user_profile_enabled: false" } " in hermes's "
                code { "config.yaml" } " so mwe-mcp is the only memory." }
            li { "Enable the hook plugins under " code { "plugins.enabled" } ": "
                code { "mwe-watchdog" } " (recommended — verifies each turn's "
                "recall block actually reaches the model) and "
                code { "mwe-media" } " (if you want media capture)." }
            li { "Restart hermes so it loads the new plugins." }
        }
    }
}

// ---------------------------------------------------------------------
// Installer generators
// ---------------------------------------------------------------------

/// Where an embedded plugin file lands on the consumer box.
enum Dest {
    /// `$HERMES_HOME/plugins/` — the out-of-tree runtime plugin dir.
    HermesHome,
    /// `$HERMES_SRC/plugins/` — inside the hermes checkout.
    HermesSrc,
}

/// Route an embedded path to its destination base + relative path.
fn route_embedded_path(rel: &str) -> Option<(Dest, String)> {
    if let Some(r) = rel.strip_prefix("memory/") {
        return Some((Dest::HermesHome, r.to_owned()));
    }
    if let Some(r) = rel.strip_prefix("gateway/") {
        return Some((Dest::HermesHome, r.to_owned()));
    }
    if let Some(r) = rel.strip_prefix("agent/") {
        return Some((Dest::HermesHome, r.to_owned()));
    }
    if rel.starts_with("context_engine/") {
        return Some((Dest::HermesSrc, rel.to_owned()));
    }
    None
}

/// Embedded plugin files in deterministic order, `(relpath, utf8)`.
fn plugin_files() -> Vec<(String, String)> {
    let mut names: Vec<String> = HermesBridge::iter()
        .map(std::borrow::Cow::into_owned)
        // Keep Python build artifacts out of the consumer's checkout:
        // CPython writes them under `__pycache__`, so dropping that dir
        // drops every `.pyc` (the sole guard — see [`HermesBridge`]).
        .filter(|rel| !rel.contains("__pycache__"))
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|rel| {
            HermesBridge::get(&rel).map(|f| (rel, String::from_utf8_lossy(&f.data).into_owned()))
        })
        .collect()
}

/// Generate the self-contained POSIX installer, or `None` for an
/// unknown consumer.
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "shell ${VAR:-default} braces are not Rust format args"
)]
fn render_install_sh(consumer: &str) -> Option<String> {
    if consumer != "hermes" {
        return None;
    }
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str(
        "# mwe-mcp hermes bridge installer — self-contained, served by your mwe-mcp server.\n",
    );
    s.push_str("# Places the four bridge plugins. It never touches your token.\n");
    s.push_str("set -eu\n\n");
    s.push_str("HERMES_HOME=\"${HERMES_HOME:-$HOME/.hermes}\"\n\n");
    s.push_str("# The context-engine plugin must land inside the hermes-agent checkout.\n");
    s.push_str("if [ -z \"${HERMES_SRC:-}\" ]; then\n");
    s.push_str("  if [ -d \"./plugins/context_engine\" ] || [ -d \"./plugins\" ]; then\n");
    s.push_str("    HERMES_SRC=\"$(pwd)\"\n");
    s.push_str("  else\n");
    s.push_str("    echo \"error: run this from inside your hermes-agent checkout, or set HERMES_SRC=/path/to/hermes-agent\" >&2\n");
    s.push_str("    exit 1\n");
    s.push_str("  fi\n");
    s.push_str("fi\n\n");

    for (rel, content) in plugin_files() {
        let Some((dest, dest_rel)) = route_embedded_path(&rel) else {
            continue;
        };
        let base = match dest {
            Dest::HermesHome => "$HERMES_HOME/plugins",
            Dest::HermesSrc => "$HERMES_SRC/plugins",
        };
        let full = format!("{base}/{dest_rel}");
        let parent = full.rsplit_once('/').map_or(full.as_str(), |(p, _)| p);
        s.push_str("mkdir -p \"");
        s.push_str(parent);
        s.push_str("\"\n");
        s.push_str("cat > \"");
        s.push_str(&full);
        s.push_str("\" <<'");
        s.push_str(SH_DELIM);
        s.push_str("'\n");
        s.push_str(&content);
        if !content.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(SH_DELIM);
        s.push('\n');
    }

    s.push_str(
        "\nprintf '%s\\n' \"\" \\\n  \"mwe-mcp hermes bridge: plugins installed.\" \\\n  \"  memory + media + watchdog -> $HERMES_HOME/plugins/\" \\\n  \"  context engine -> $HERMES_SRC/plugins/context_engine/\" \\\n  \"\" \\\n  \"Four steps remain — they are yours (the installer never handles your token):\" \\\n  \"  1. Issue a token from your mwe-mcp dashboard home and set MWE_TOKEN in hermes's .env.\" \\\n  \"  2. Disable hermes's built-in memory (memory_enabled: false AND user_profile_enabled: false) so mwe-mcp is the only memory.\" \\\n  \"  3. Enable the hook plugins in config.yaml plugins.enabled: mwe-watchdog (recommended) and mwe-media (if you want media capture).\" \\\n  \"  4. Restart hermes so it loads the new plugins.\"\n",
    );
    Some(s)
}

/// Generate the self-contained PowerShell installer.
fn render_install_ps1(consumer: &str) -> Option<String> {
    if consumer != "hermes" {
        return None;
    }
    let mut s = String::new();
    s.push_str("# mwe-mcp hermes bridge installer (Windows / PowerShell) — self-contained.\n");
    s.push_str("# Places the four bridge plugins. It never touches your token.\n");
    s.push_str("$ErrorActionPreference = \"Stop\"\n\n");
    s.push_str("if ($env:HERMES_HOME) { $HermesHome = $env:HERMES_HOME } else { $HermesHome = Join-Path $HOME \".hermes\" }\n");
    s.push_str("if ($env:HERMES_SRC) { $HermesSrc = $env:HERMES_SRC }\n");
    s.push_str("elseif (Test-Path \"./plugins\") { $HermesSrc = (Get-Location).Path }\n");
    s.push_str("else { Write-Error \"run this from inside your hermes-agent checkout, or set HERMES_SRC\"; exit 1 }\n\n");
    s.push_str("function Write-PluginFile($RelBase, $Rel, $Content) {\n");
    s.push_str("  $path = Join-Path $RelBase $Rel\n");
    s.push_str("  $dir = Split-Path -Parent $path\n");
    s.push_str("  New-Item -ItemType Directory -Force -Path $dir | Out-Null\n");
    s.push_str("  Set-Content -LiteralPath $path -Value $Content\n");
    s.push_str("}\n\n");

    for (rel, content) in plugin_files() {
        let Some((dest, dest_rel)) = route_embedded_path(&rel) else {
            continue;
        };
        let base_var = match dest {
            Dest::HermesHome => "(Join-Path $HermesHome \"plugins\")",
            Dest::HermesSrc => "(Join-Path $HermesSrc \"plugins\")",
        };
        s.push_str("Write-PluginFile ");
        s.push_str(base_var);
        s.push_str(" \"");
        s.push_str(&dest_rel);
        s.push_str("\" @'\n");
        s.push_str(&content);
        if !content.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("'@\n");
    }

    s.push_str("\nWrite-Host \"\"\n");
    s.push_str("Write-Host \"mwe-mcp hermes bridge: plugins installed.\"\n");
    s.push_str("Write-Host \"Four steps remain — they are yours (the installer never handles your token):\"\n");
    s.push_str("Write-Host \"  1. Issue a token from your mwe-mcp dashboard home and set MWE_TOKEN in hermes's .env.\"\n");
    s.push_str("Write-Host \"  2. Disable hermes's built-in memory (memory_enabled: false and user_profile_enabled: false).\"\n");
    s.push_str("Write-Host \"  3. Enable the hook plugins in config.yaml plugins.enabled: mwe-watchdog (recommended) and mwe-media (for media capture).\"\n");
    s.push_str("Write-Host \"  4. Restart hermes so it loads the new plugins.\"\n");
    Some(s)
}

/// Machine-readable instructions an agent is pointed at ("Read … and
/// follow"). `origin` is the request-derived public origin. Dispatches
/// per consumer; `None` for one without a served `install.md`.
fn render_install_md(consumer: &str, origin: &str) -> Option<String> {
    match consumer {
        "hermes" => Some(render_install_md_hermes(origin)),
        "claude-code" => Some(render_install_md_claude_code(origin)),
        _ => None,
    }
}

/// Agent-driven install for the **Claude Code** smart-consumer bridge.
/// No plugins, no token: Claude Code registers the MCP server, signs in
/// over OAuth, and optionally adds a token-less session-start nudge. It
/// never bootstraps a project wiki on its own — that is a separate,
/// explicit user request later.
// The body is one cohesive markdown document; splitting the string literal across
// helper fns to satisfy the line lint would only hurt readability.
#[allow(clippy::too_many_lines)]
fn render_install_md_claude_code(origin: &str) -> String {
    format!(
        "# Connect Claude Code to this mwe-mcp memory (OAuth — no token)\n\
         \n\
         You are **Claude Code**, connecting yourself to a first-party mwe-mcp\n\
         memory server at `{origin}` as a **smart consumer**. You sign in over\n\
         **OAuth** — there is **no token to paste**. Do the steps in order; do\n\
         **not** bootstrap any project's wiki as part of this install (that is a\n\
         separate, explicit request later).\n\
         \n\
         ## 1. Register the MCP server (user scope)\n\
         \n\
         ```sh\n\
         claude mcp add --transport http mwe-mcp {origin}/mcp --scope user\n\
         ```\n\
         \n\
         `--scope user` so the server resolves in every session (the optional\n\
         session-start hook below references it).\n\
         \n\
         ## 2. Sign in over OAuth — a human step: STOP and ask the operator\n\
         \n\
         The OAuth sign-in needs a browser login + approval, so it must be driven by\n\
         the **human operator in their own Claude Code session** — **not** by you as a\n\
         subprocess. If you run `claude mcp login` yourself, your process can't hold\n\
         the loopback callback while the human approves in the browser, so the\n\
         callback fails (\"not found\"). **Stop here and ask the operator** to do one\n\
         of:\n\
         \n\
         - in their interactive Claude Code, type `/mcp`, pick `mwe-mcp`, and choose\n\
           **Authenticate**; or\n\
         - in a terminal, run `claude mcp login mwe-mcp`.\n\
         \n\
         The browser opens at `{origin}`; the operator logs in as themselves and\n\
         **Approves**. No token is minted or pasted. Once approved you are connected\n\
         and a dedicated **operational wiki** is forged for this connection.\n\
         \n\
         **Then reload the session.** Claude Code loads an MCP server's tools at\n\
         session start, so if you connected mid-session the mwe-mcp tools\n\
         (`skill_fetch`, `smart_bootstrap`, etc.) are **not callable yet** — \"Connected\"\n\
         alone does not load them into the running session. Restart / reload Claude\n\
         Code (or open a fresh session) and they appear; every later session picks\n\
         them up automatically.\n\
         \n\
         ## 3. Load your skill\n\
         \n\
         *(This needs the mwe-mcp tools to be live — see the reload note in step 2. If\n\
         `skill_fetch` is not available yet, reload the session first.)*\n\
         \n\
         Call `skill_fetch` for `core` and follow its dispatcher — it routes you to\n\
         `smart-consumer` / `smart-codebase` inside a project, or `core-globalmemory`\n\
         for transversal recall. You have the full Local tool surface, `skill_fetch`\n\
         included.\n\
         \n\
         ## 4. Session-start recall hook — strongly recommended (a human step)\n\
         \n\
         **Without this hook you may not auto-recall or auto-capture at all** —\n\
         model-driven recall alone tends to stay idle until the user asks (the same way\n\
         claude.ai does not touch this memory unless told to). The hook is what makes\n\
         recall + capture fire at the start of **every** session. But merging an\n\
         externally-sourced hook into\n\
         `~/.claude/settings.json` is **self-modification with external content**, which\n\
         Claude Code's safety classifier blocks — so the **operator must add it, not\n\
         you.** Point the operator at `{origin}/connect/hooks/claude-code.json` and ask\n\
         them to merge its `snippet.hooks` into `~/.claude/settings.json` themselves. It\n\
         is token-less, holds no secret, and only injects a fixed reminder to call\n\
         `smart_bootstrap` + recall at session start. (If you are running with\n\
         **bypass-permissions** — the operator started Claude Code with\n\
         `--dangerously-skip-permissions` — the classifier does not stop you, so you\n\
         *can* merge it yourself; do so only then, and tell the operator you did.)\n\
         \n\
         ## 5. How your memory is organised — route, never dump\n\
         \n\
         - Your **operational wiki** (forged at sign-in): general working memory,\n\
           your behaviour rules, and a `conversations.md` log — authored with\n\
           `wiki_admin_*`.\n\
         - **Per-project wikis**: durable project / codebase knowledge, authored\n\
           when you bootstrap a project (separate, explicit, never automatic).\n\
         - The **user's personal memory** (their standard wikis): facts about the\n\
           user (preferences, appointments, people) → send via `wiki_ingest_message`,\n\
           not your own wiki.\n\
         \n\
         Route every captured item by this split; **never dump everything into the\n\
         user's standard memory**. The `core` / `smart-consumer` skill carries the\n\
         full routing rules and the auto-recall / auto-capture posture.\n\
         \n\
         ## 6. Do NOT bootstrap a project now\n\
         \n\
         The install ends here. When the user later asks (or you judge a moment to be\n\
         durable project knowledge), follow `smart-consumer` / `smart-codebase`:\n\
         never scan folders on your own, leave the local copy intact, and for an\n\
         existing `docs/` or wiki **propose** the path (build an mwe-style wiki from\n\
         `docs/`, or check an existing wiki for mwe-compatibility then ingest),\n\
         surfacing the `CLAUDE.md` / `AGENTS.md` documentation-rules decision. Keep\n\
         the user aware of every action.\n\
         \n\
         ## Keeping a repo private (per-project isolation)\n\
         \n\
         Memory is active in every Claude Code session on this machine. To opt a\n\
         project out entirely — no recall, no save, nothing leaves it — add to that\n\
         repo's `.claude/settings.json`:\n\
         \n\
         ```json\n\
         {{ \"mcpServers\": {{ \"mwe-mcp\": null }} }}\n\
         ```\n\
         \n\
         A work repo backed by a *different* memory server instead points its own\n\
         `.mcp.json` / settings at that server's origin. See `INTEGRATING.md`\n\
         (\"Per-project isolation\") for the deployment topology.\n"
    )
}

/// Agent-driven install for the **hermes** standard-consumer bridge —
/// points the agent at the served `curl … | sh` plugin installer.
fn render_install_md_hermes(origin: &str) -> String {
    format!(
        "# Install the mwe-mcp hermes bridge\n\
         \n\
         You are connecting **hermes** to a first-party mwe-mcp memory server at `{origin}`.\n\
         This installer is served by that same server. Run it from **inside your\n\
         hermes-agent checkout** (the context-engine plugin must land there):\n\
         \n\
         ```sh\n\
         curl -fsSL {origin}/bridges/hermes/install.sh | sh\n\
         ```\n\
         \n\
         Windows (PowerShell): `irm {origin}/bridges/hermes/install.ps1 | iex`\n\
         \n\
         That places the four plugins. It does **not** touch the token. Once the\n\
         files are in place, **tell your operator** to do these four things — do not\n\
         attempt them silently, and do not handle the token yourself:\n\
         \n\
         1. Issue a token from the mwe-mcp dashboard home and set `MWE_TOKEN` in\n\
            hermes's `.env`.\n\
         2. Disable hermes's built-in memory: set `memory_enabled: false` (the\n\
            bot's MEMORY.md) AND `user_profile_enabled: false` (the user's\n\
            USER.md) — two separate flags — so mwe-mcp is the single governed\n\
            memory.\n\
         3. Enable the hook plugins in `config.yaml` under `plugins.enabled`:\n\
            `mwe-watchdog` (recommended — verifies each turn's recall block\n\
            actually reaches the model and logs loudly when the host drops it)\n\
            and `mwe-media` (if you want media capture).\n\
         4. Restart hermes so it loads the new plugins.\n"
    )
}

// ---------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------

fn text_response(body: String, content_type: &'static str) -> Response {
    let mut resp = body.into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300, must-revalidate"),
    );
    resp
}

async fn install_sh(Path(consumer): Path<String>) -> Response {
    render_install_sh(&consumer).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |body| text_response(body, "text/plain; charset=utf-8"),
    )
}

async fn install_ps1(Path(consumer): Path<String>) -> Response {
    render_install_ps1(&consumer).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |body| text_response(body, "text/plain; charset=utf-8"),
    )
}

async fn install_md(Path(consumer): Path<String>, Host(host): Host) -> Response {
    render_install_md(&consumer, &origin_from_host(&host)).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |body| text_response(body, "text/markdown; charset=utf-8"),
    )
}

// --- public (anonymous shell) ---
//
// Informational onboarding pages, not forms: they use the reading-width
// shell (a centered ~52rem column) rather than the narrow 30rem login-form
// width — the splash copy, the consumer table, and the long `mcp add`
// command blocks all want the extra room.

async fn front_page() -> Html<String> {
    Html(layout::anonymous_reading_page("mwe-mcp", &front_body("")))
}

async fn public_bridges_index(Host(host): Host) -> Html<String> {
    Html(layout::anonymous_reading_page(
        "Bridges",
        &catalog_body("", &origin_from_host(&host)),
    ))
}

async fn public_bridge_page(Path(consumer): Path<String>, Host(host): Host) -> Response {
    bridge_label(&consumer).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |label| {
            Html(layout::anonymous_reading_page(
                &format!("{label} bridge"),
                &guide_body(&consumer, &origin_from_host(&host)),
            ))
            .into_response()
        },
    )
}

// --- dashboard tab (authenticated shell, links resolve under /dashboard) ---

async fn tab_bridges_index(user: SessionUser, Host(host): Host) -> Html<String> {
    Html(layout::authenticated_page(
        "Bridges",
        &user,
        &catalog_body("/dashboard", &origin_from_host(&host)),
    ))
}

async fn tab_bridge_page(
    Path(consumer): Path<String>,
    Host(host): Host,
    user: SessionUser,
) -> Response {
    bridge_label(&consumer).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |label| {
            Html(layout::authenticated_page(
                &format!("{label} bridge"),
                &user,
                &guide_body(&consumer, &origin_from_host(&host)),
            ))
            .into_response()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn embed_excludes_pycache() {
        for (rel, _) in plugin_files() {
            assert!(
                !rel.contains("__pycache__"),
                "embedded a __pycache__ path: {rel}"
            );
        }
        let rels: Vec<String> = plugin_files().into_iter().map(|(r, _)| r).collect();
        assert!(
            rels.iter().any(|r| r.starts_with("memory/mwe/")),
            "memory/mwe missing: {rels:?}"
        );
        assert!(
            rels.iter().any(|r| r.starts_with("gateway/mwe-media/")),
            "gateway/mwe-media missing"
        );
        assert!(
            rels.iter()
                .any(|r| r.starts_with("context_engine/mwe-truncate/")),
            "context_engine/mwe-truncate missing"
        );
        assert!(
            rels.iter().any(|r| r.starts_with("agent/mwe-watchdog/")),
            "agent/mwe-watchdog missing"
        );
    }

    #[test]
    fn delimiter_never_collides_with_plugin_source() {
        for (rel, content) in plugin_files() {
            for line in content.lines() {
                assert_ne!(
                    line, SH_DELIM,
                    "{rel}: a line equals the sh heredoc delimiter"
                );
                assert!(
                    !line.starts_with("'@"),
                    "{rel}: a line starts with the PowerShell here-string terminator"
                );
            }
        }
    }

    #[test]
    fn install_sh_is_self_contained_and_routes_destinations() {
        let sh = render_install_sh("hermes").expect("hermes sh");
        assert!(sh.contains("cat > \"$HERMES_HOME/plugins/mwe/"));
        assert!(sh.contains("cat > \"$HERMES_HOME/plugins/mwe-media/"));
        assert!(sh.contains("cat > \"$HERMES_HOME/plugins/mwe-watchdog/"));
        assert!(sh.contains("cat > \"$HERMES_SRC/plugins/context_engine/mwe-truncate/"));
        assert!(sh.contains("HERMES_HOME=\"${HERMES_HOME:-$HOME/.hermes}\""));
        assert!(sh.contains("HERMES_SRC=\"$(pwd)\""));
        assert!(sh.contains("Issue a token"));
        assert!(sh.contains("memory_enabled: false"));
        assert!(sh.contains("user_profile_enabled: false"));
        assert!(sh.contains("Restart hermes"));
        assert!(!sh.contains("__pycache__"));
        assert!(!sh.contains(".pyc"));
    }

    #[test]
    fn install_ps1_is_self_contained() {
        let ps = render_install_ps1("hermes").expect("hermes ps1");
        assert!(ps.contains("$HermesHome"));
        assert!(ps.contains("$HermesSrc"));
        assert!(ps.contains("Join-Path $HermesSrc \"plugins\""));
        assert!(ps.contains("mwe-watchdog"));
        assert!(ps.contains("memory_enabled: false"));
        assert!(ps.contains("user_profile_enabled: false"));
    }

    #[test]
    fn install_md_carries_origin_and_residual_steps() {
        let md = render_install_md("hermes", "https://memory.anna.dev").expect("md");
        assert!(md.contains("curl -fsSL https://memory.anna.dev/bridges/hermes/install.sh | sh"));
        assert!(md.contains("irm https://memory.anna.dev/bridges/hermes/install.ps1 | iex"));
        assert!(md.contains("tell your operator"));
        assert!(md.contains("MWE_TOKEN"));
        assert!(md.contains("memory_enabled: false"));
        assert!(md.contains("user_profile_enabled: false"));
        assert!(md.contains("mwe-watchdog"));
        assert!(md.contains("plugins.enabled"));
    }

    #[test]
    fn unknown_consumer_has_no_installer() {
        assert!(render_install_sh("nope").is_none());
        assert!(render_install_ps1("nope").is_none());
        assert!(render_install_md("nope", "https://x").is_none());
    }

    #[test]
    fn front_body_points_agent_at_catalog_and_human_at_signin() {
        let html = front_body("").into_string();
        assert!(html.contains("If you are an agent"));
        assert!(html.contains("/bridges"));
        assert!(html.contains("href=\"/dashboard/\""));
    }

    #[test]
    fn catalog_lists_hermes_with_agent_instructions_link() {
        let pub_html = catalog_body("", "https://memory.anna.dev").into_string();
        assert!(pub_html.contains("hermes"));
        assert!(pub_html.contains("agent instructions"));
        assert!(pub_html.contains("/bridges/hermes/install.md"));
        assert!(pub_html.contains("href=\"/bridges/hermes\""));
        // The bridge-less claude.ai section shows the MCP URL to paste + the
        // skill-upload funnel.
        assert!(pub_html.contains("Connect the claude.ai web app"));
        assert!(pub_html.contains("https://memory.anna.dev/mcp"));
        assert!(pub_html.contains("/webagentoauth/skill.md"));
        // Under the dashboard the guide link is prefixed, the install.md is not.
        let tab_html = catalog_body("/dashboard", "https://memory.anna.dev").into_string();
        assert!(tab_html.contains("href=\"/dashboard/bridges/hermes\""));
        assert!(tab_html.contains("/bridges/hermes/install.md"));
    }

    #[test]
    fn guide_has_install_command_and_no_inline_token_mint() {
        let html = guide_body("hermes", "https://memory.anna.dev").into_string();
        assert!(html.contains("https://memory.anna.dev/bridges/hermes/install.sh"));
        assert!(html.contains("install.md"));
        assert!(html.contains("memory_enabled: false"));
        assert!(html.contains("user_profile_enabled: false"));
        assert!(html.contains("mwe-watchdog"));
        assert!(html.contains("Restart hermes"));
        // The token is issued from the home, not minted here.
        assert!(html.contains("dashboard home"));
    }

    #[test]
    fn claude_code_guide_is_smart_and_has_no_curl_installer() {
        let html = guide_body("claude-code", "https://memory.anna.dev").into_string();
        assert!(html.contains("smart consumer"));
        assert!(html.contains("claude mcp add"));
        assert!(html.contains("--scope user"));
        // No curl|sh plugin installer for the smart consumer.
        assert!(!html.contains("install.sh | sh"));
        // The per-project privacy switch is surfaced on the page (maud
        // HTML-escapes the quotes, so match the distinctive token).
        assert!(html.contains("mcpServers"));
        // install.md only — claude-code ships no shell installers.
        assert!(render_install_sh("claude-code").is_none());
        assert!(render_install_ps1("claude-code").is_none());
    }

    #[test]
    fn claude_code_install_md_uses_oauth_no_token_and_forbids_auto_bootstrap() {
        let md = render_install_md("claude-code", "https://memory.anna.dev")
            .expect("claude-code install.md");
        assert!(md.contains("https://memory.anna.dev/mcp"));
        assert!(md.contains("claude mcp add"));
        assert!(md.contains("--scope user"));
        // OAuth — no token is ever pasted (no Bearer header).
        assert!(md.contains("OAuth"));
        assert!(!md.contains("Bearer"));
        // The optional token-less session-start nudge.
        assert!(md.contains("/connect/hooks/claude-code.json"));
        // Bootstrap is never automatic.
        assert!(md.contains("separate, explicit"));
        // Per-project isolation switch.
        assert!(md.contains("\"mwe-mcp\": null"));
    }

    #[test]
    fn catalog_lists_claude_code_with_agent_instructions() {
        let html = catalog_body("", "https://memory.anna.dev").into_string();
        assert!(html.contains("Claude Code (Anthropic)"));
        assert!(html.contains("href=\"/bridges/claude-code\""));
        assert!(html.contains("/bridges/claude-code/install.md"));
    }

    #[tokio::test]
    async fn public_routes_serve_pages_and_scripts() {
        let cases = [
            ("/", "If you are an agent"),
            ("/bridges", "agent instructions"),
        ];
        for (uri, needle) in cases {
            let resp = public_site_router()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("host", "memory.anna.dev")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            assert!(body_string(resp).await.contains(needle), "{uri}");
        }

        let resp = public_site_router()
            .oneshot(
                Request::builder()
                    .uri("/bridges/hermes/install.sh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp).await.starts_with("#!/bin/sh"));
    }

    #[tokio::test]
    async fn public_bridge_page_localhost_uses_http_scheme() {
        let resp = public_site_router()
            .oneshot(
                Request::builder()
                    .uri("/bridges/hermes")
                    .header("host", "127.0.0.1:8742")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            body_string(resp)
                .await
                .contains("http://127.0.0.1:8742/bridges/hermes/install.sh")
        );
    }

    #[tokio::test]
    async fn unknown_consumer_endpoints_404() {
        for uri in [
            "/bridges/nope",
            "/bridges/nope/install.sh",
            "/bridges/nope/install.md",
        ] {
            let resp = public_site_router()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("host", "x")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri} should 404");
        }
    }
}
