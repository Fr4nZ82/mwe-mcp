// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bridge **onboarding + distribution** surface.
//!
//! The same non-secret content is reachable two ways, so we never have
//! to ask "who is visiting":
//!
//! - **Public** (root-mounted, anonymous) for agents and `curl`:
//!   - `GET /` — slim product front page: a line pointing a consumer at
//!     the bridge catalog, and a sign-in link for the human.
//!   - `GET /bridges` + `GET /bridges/:consumer` — the catalog and the
//!     per-bridge install guide. Every entry carries an **"instructions it
//!     can follow"** link straight to its machine-readable `install.md`,
//!     so a consumer doesn't need the human guide at all.
//!   - `GET /bridges/:consumer/install.{sh,ps1,md}` — the self-contained
//!     installers (each bridge's file tree embedded via [`rust_embed`] and
//!     inlined as heredocs / here-strings; one `curl … | sh`, no
//!     `tar`/`jq`/bundle). A consumer ships only the forms that can work:
//!     claude-code has just an `install.md`, and nanoclaw no `.ps1`.
//! - **Dashboard tab** (`/dashboard/bridges`, authenticated) for the
//!   operator: the *same* catalog + guide bodies wrapped in the dashboard
//!   shell, so "Bridges" sits in the nav next to Wikis / Facts / Settings.
//!   Shared body functions take a base prefix so the in-page links resolve
//!   under `/dashboard` there and at the root publicly.
//!
//! The **token never lives here** — it is a credential, minted on the
//! dashboard's Tokens page, which the home's "Connect a consumer" card
//! links to. These pages and scripts only ever instruct the operator to
//! mint it, disable the host's built-in memory, and restart.

use axum::Router;
use axum::extract::{Host, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use maud::{Markup, html};
use rust_embed::RustEmbed;

use crate::auth::SessionUser;
use crate::state::DashboardState;
use crate::ui::layout;

/// The hermes bridge tree (plugins + gateway hooks + cron scripts),
/// embedded from the in-repo bridge directory. Python build artifacts
/// (`__pycache__`/`*.pyc`) are filtered out in [`bridge_files`] rather
/// than via rust-embed's `#[exclude]` (which would pull in the
/// `include-exclude` feature and its glob dependencies for no real
/// gain), so they never reach a consumer's checkout; non-runtime files
/// (README, smokes, the manifest) are dropped by
/// [`route_embedded_path`] returning `None`.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../agents-bridges/hermes/"]
struct HermesBridge;

/// The nanoclaw bridge tree, embedded from the in-repo bridge
/// directory. Exactly two of its directories travel to a fork — the
/// `mwe` agent template and the `add-mwe-memory` skill; everything else
/// (README, manifest, smokes and their stubs) is dropped by
/// [`route_embedded_path`] returning `None`. The manifest is embedded
/// too, unrouted, because [`nanoclaw_upstream`] reads the tested repo and
/// ref out of it so the installer cannot drift from the bridge.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../agents-bridges/nanoclaw/"]
struct NanoclawBridge;

/// Catalog of bridged consumers, in display order. A consumer appears
/// here only when its bridge ships a served onboarding surface — a
/// `curl … | sh` installer (nanoclaw, hermes) **or** an agent-driven
/// `install.md` (claude-code).
///
/// nanoclaw leads: it is the ready-made assistant, the one consumer an
/// operator with no agent of their own can install and talk to.
const BRIDGES: &[(&str, &str)] = &[
    ("nanoclaw", "NanoClaw (nanoco)"),
    ("hermes", "Hermes (Nous Research)"),
    ("claude-code", "Claude Code (Anthropic)"),
];

/// Heredoc / here-string delimiter for the inlined bridge files. Chosen
/// so it cannot appear in the Python, YAML, TypeScript, JSON or Markdown
/// a bridge ships; asserted by a test over every embedded tree.
const SH_DELIM: &str = "MWE_BRIDGE_EOF";

/// nanoclaw's registry branches. Its channel and provider skills carry
/// no payload of their own: each copies its files with `git show
/// origin/<branch>:<path>` (the `nc:copy from-branch:` directive). A
/// shallow clone of one ref tracks only that ref, so the installer names
/// these branches itself and the fork can install a channel.
const NANOCLAW_REGISTRY_BRANCHES: &[&str] = &["channels", "providers"];

/// The template pick the setup wizard reads out of the fork's `.env`
/// (`setup/auto.ts` bridges this one key, and only this one, into the
/// run). Written when absent so the wizard offers `mwe` instead of
/// making the operator find it in the picker.
const NANOCLAW_TEMPLATE_ENV_LINE: &str = "NANOCLAW_TEMPLATE_PATH=mwe";

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

/// The Tokens page, named the same way everywhere it is mentioned.
///
/// A link only for a reader the console will let in: it is admin-only,
/// so pointing anybody else at it is a 403 with extra steps, and on the
/// anonymous public page there is no session to judge by at all. The
/// sentence around it reads the same either way, so nothing is lost.
fn tokens_page(linked: bool) -> Markup {
    html! {
        @if linked {
            a href="/dashboard/tokens" { "Tokens" } " page"
        } @else {
            strong { "Tokens" } " page of this dashboard (an admin's)"
        }
    }
}

/// Front page body. `base` prefixes the in-app catalog link.
fn front_body(base: &str) -> Markup {
    html! {
        p.muted {
            "Governed, persistent memory over MCP, for any consumer — the bot or "
            "assistant that talks to it. Your memory stays a folder you control; "
            "consumers connect over HTTP."
        }
        h2 { "If you are a consumer" }
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
/// "instructions for the consumer" link always points at the public
/// `install.md`.
/// `origin` (`scheme://host`) is shown in the bridge-less claude.ai section.
fn catalog_body(base: &str, origin: &str, may_mint: bool) -> Markup {
    html! {
        p.muted {
            "A bridge connects a consumer — the bot or assistant that talks to "
            "this memory — to mwe-mcp. Start with "
            strong { "NanoClaw" } " if you have no consumer yet: it is the "
            "ready-made assistant, installed with one command and arriving "
            "with this memory as its only memory. It and "
            strong { "hermes" } " are the two " strong { "standard" }
            " consumers, wired at full fidelity — one ingest per turn, recall "
            "block, per-sender attribution. A "
            strong { "smart" } " consumer (Claude Code) brings its own LLM and "
            "authors a project's smart wiki over MCP. Pick your host for the "
            "setup, or hand the consumer its " code { "install.md" } " directly."
        }
        table {
            thead { tr {
                th { "Consumer" }
                th { "Setup" }
                th { "For the consumer to read" }
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
                                "instructions it can follow"
                            }
                        }
                    }
                }
            }
        }

        (claude_ai_section(origin, may_mint))
    }
}

/// Instructions for connecting the **claude.ai web app** — not a bridge: it has
/// no local install, it connects as a *smart* consumer over the `webagentoauth`
/// OAuth flow (no token to copy). Authors its own dedicated wiki; recalls and
/// saves on request. The exact field names in claude.ai's UI may differ — this
/// is the manual path until it is verified live against the public endpoint.
fn claude_ai_section(origin: &str, may_mint: bool) -> Markup {
    html! {
        h2 { "Connect the claude.ai web app" }
        p.muted {
            "claude.ai (Pro / Max / Team) connects directly as a smart consumer "
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
            (tokens_page(may_mint)) "."
        }
    }
}

/// Per-consumer install guide body. No token here — that lives on the
/// dashboard home's "Connect a consumer" card. Dispatches on the
/// consumer: nanoclaw and hermes ship a `curl … | sh` installer;
/// claude-code is an agent-driven `install.md` (no files, no shell
/// installer).
fn guide_body(consumer: &str, origin: &str, may_mint: bool) -> Markup {
    match consumer {
        "nanoclaw" => nanoclaw_guide_body(origin, may_mint),
        "claude-code" => claude_code_guide_body(origin),
        _ => hermes_guide_body(consumer, origin, may_mint),
    }
}

/// Human guide for the **nanoclaw** bridge — the ready-made assistant,
/// and the first consumer this catalog recommends. One command places
/// the `mwe` agent template and the `add-mwe-memory` fork skill; the
/// five steps after it are the operator's, and the token is one of them.
///
/// No PowerShell here on purpose: nanoclaw runs on Windows only inside
/// WSL2, so the Windows path is the same `sh` command in a WSL2 shell —
/// see [`render_install_ps1`].
fn nanoclaw_guide_body(origin: &str, may_mint: bool) -> Markup {
    let curl = format!("curl -fsSL {origin}/bridges/nanoclaw/install.sh | sh");
    let agent_line = format!(
        "Read {origin}/bridges/nanoclaw/install.md and follow the instructions to connect me to this memory."
    );
    html! {
        p.muted {
            strong { "Start here if you have no consumer yet." } " A consumer is "
            "the bot or assistant that talks to this memory. NanoClaw is the "
            "ready-made assistant: a chat assistant that arrives with this memory "
            "already wired as its " strong { "only" } " memory. Its built-in "
            "memory stays off and no session is carried between turns, so what "
            "it remembers is exactly what the memory recalls — per person, and "
            "governed. Everything else NanoClaw brings (chat, the web, "
            "its own container, scheduled tasks, several channels at once) is "
            "untouched."
        }

        h2 { "1. Install the template and the skill" }
        p { "Run this anywhere. If you are already inside a NanoClaw checkout it "
            "uses that one; otherwise it clones NanoClaw at the tested ref into "
            code { "~/nanoclaw" } " (set " code { "NANOCLAW_DIR" }
            " to put it elsewhere, or to point at a fork you already have):" }
        pre.endpoint-display { (curl) }
        p.muted {
            "Where the files land: the " code { "mwe" } " agent template in "
            code { "templates/mwe/" } " and the " code { "add-mwe-memory" }
            " fork skill in " code { ".claude/skills/add-mwe-memory/" }
            ", both inside the checkout. The installer itself needs only "
            code { "git" } " — NanoClaw's own "
            code { "nanoclaw.sh" } " installs Node, pnpm and Docker if they are "
            "missing. Claude Code is what drives the skill conversationally in "
            "step 2. The installer also fetches NanoClaw's "
            code { "channels" } " and " code { "providers" } " branches — its "
            "channel adapters are copied out of them, so without those a "
            "channel cannot be installed at all — and names " code { "mwe" }
            " as the template in the checkout's " code { ".env" } ", so setup "
            "offers it instead of asking you to find it."
        }
        p.muted {
            strong { "Windows:" } " NanoClaw runs under WSL2, so there is no "
            "PowerShell installer. Open your WSL2 shell and run the command "
            "above there."
        }

        h3 { "…or let a consumer do it" }
        p { "Paste this to any consumer that already has a shell — it runs the "
            "same installer and then hands you the steps below:" }
        pre.endpoint-display { (agent_line) }
        p.muted {
            "Machine-readable form: "
            a href="/bridges/nanoclaw/install.md" { "/bridges/nanoclaw/install.md" }
        }

        h2 { "2. Finish (the steps the installer leaves to you)" }
        p { "The installer never touches your token. From the checkout:" }
        ol {
            li {
                "Stamp the agent. On a fresh install run " code { "bash nanoclaw.sh" }
                " and confirm the " code { "mwe" } " template it offers (decline, "
                "and you pick it by hand: " strong { "Local templates" } ", then "
                code { "mwe" } "). On an install that already has agents: "
                code { "ncl groups create --template mwe --name mwe --new" }
                " — " code { "--name" } " is yours, it becomes the group folder. "
                "It is not what the assistant answers to: its name is a fact of "
                "this memory, and anybody it serves can tell it in chat."
            }
            li {
                "Apply the skill: " code { "/add-mwe-memory" } " from Claude Code. "
                "Without Claude Code, the same steps are ordinary shell commands "
                "in " code { ".claude/skills/add-mwe-memory/SKILL.md" }
                ". Its last step restarts the agent containers as well as the "
                "service — a container that keeps running answers with the code "
                "as it was before, and says nothing about it."
            }
            li {
                "Issue a " strong { "standard" } " consumer token from the "
                (tokens_page(may_mint)) " and set it as "
                code { "MWE_TOKEN" } " in the checkout's " code { ".env" }
                ". In that consumer's delegations tick every person it will "
                "speak for, plus " code { "guest" } " — without " code { "guest" }
                " an unrecognised sender is refused instead of answered "
                "anonymously."
            }
            li {
                "Fill in " code { "senderMap" } " in " code { "mwe.json" }
                " — one line per person, " code { "<channel>:<platform id>" }
                " to their mwe user id — and restart NanoClaw. Anyone not listed "
                "speaks as a guest; there is no fallback to the owner."
            }
            li { "Connect a channel (" code { "/manage-channels" }
                ", or " code { "ncl wirings create" } ") and talk to it." }
        }
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
            "its own subscription LLM and speaks MCP itself, so there are no "
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

        h2 { "Let Claude Code do the setup parts" }
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
            " — Claude Code can't merge an external hook into its own settings "
            "(it blocks that as self-modification), unless it is running in "
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
fn hermes_guide_body(consumer: &str, origin: &str, may_mint: bool) -> Markup {
    let curl = format!("curl -fsSL {origin}/bridges/{consumer}/install.sh | sh");
    let ps = format!("irm {origin}/bridges/{consumer}/install.ps1 | iex");
    let agent_line = format!(
        "Read {origin}/bridges/{consumer}/install.md and follow the instructions to connect me to this memory."
    );
    html! {
        p.muted {
            "First-party, served by this server. Follow it by hand below, or "
            "hand it to hermes itself."
        }

        h2 { "1. Install the plugins" }
        p { "Run this " strong { "from inside your hermes-agent checkout" }
            " (the context-engine plugin must land there). Linux / macOS:" }
        pre.endpoint-display { (curl) }
        p.muted { "Windows (PowerShell): " code { (ps) } }
        p.muted {
            "Where the files land: the memory + media + watchdog plugins go to "
            code { "~/.hermes/plugins/" } ", the reverse-channel gateway hook to "
            code { "~/.hermes/hooks/mwe-events/" }
            " (auto-discovered — it delivers " code { "fact_minted_for_you" }
            " and " code { "reminder_due" }
            " notices to their recipients), the daily-digest script to "
            code { "~/.hermes/scripts/" } ", and the context-engine plugin goes "
            "into the hermes-agent checkout you run the command from. You normally "
            "don't set anything — but if your layout differs, two environment "
            "variables override the defaults: " code { "HERMES_HOME" }
            " (hermes's runtime dir, default " code { "~/.hermes" } ") and "
            code { "HERMES_SRC" }
            " (your hermes-agent checkout, if you don't run the installer from inside it)."
        }

        h3 { "…or let hermes do it" }
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
            li { "Issue a " strong { "standard" } " consumer token from the "
                (tokens_page(may_mint)) " and set it as "
                code { "MWE_TOKEN" } " in hermes's " code { ".env" } "." }
            li { "Set " code { "memory_enabled: false" } " and "
                code { "user_profile_enabled: false" } " in hermes's "
                code { "config.yaml" } " so mwe-mcp is the only memory." }
            li { "Enable the hook plugins under " code { "plugins.enabled" } ": "
                code { "mwe-watchdog" } " (recommended — verifies each turn's "
                "recall block actually reaches the model) and "
                code { "mwe-media" } " (if you want media capture)." }
            li { "Restart hermes so it loads the new plugins." }
            li { "Optional — the daily memory digest: "
                code { "hermes cron create \"0 9 * * *\" … --script mwe-daily-digest.py --deliver telegram" }
                " (the full prompt is in the script's header and the bridge README)." }
        }
    }
}

// ---------------------------------------------------------------------
// Installer generators
// ---------------------------------------------------------------------

/// Where an embedded bridge file lands on the consumer box.
enum Dest {
    /// Under `$HERMES_HOME/` — the out-of-tree runtime dir (the
    /// relative path carries its own `plugins/` / `hooks/` /
    /// `scripts/` prefix).
    HermesHome,
    /// Under `$HERMES_SRC/` — inside the hermes checkout.
    HermesSrc,
    /// Under `$NANOCLAW_DIR/` — inside the nanoclaw checkout, which is
    /// the one destination that bridge has.
    NanoclawDir,
}

impl Dest {
    /// The shell variable the POSIX installer writes this destination
    /// through. Each bridge's routing only ever yields its own
    /// destinations, so one table serves every installer.
    const fn sh_var(&self) -> &'static str {
        match self {
            Self::HermesHome => "$HERMES_HOME",
            Self::HermesSrc => "$HERMES_SRC",
            Self::NanoclawDir => "$NANOCLAW_DIR",
        }
    }
}

/// Route an embedded path to its destination base + relative path.
/// `None` drops non-runtime files (README, smokes, the manifest).
fn route_embedded_path(consumer: &str, rel: &str) -> Option<(Dest, String)> {
    match consumer {
        "nanoclaw" => route_nanoclaw_path(rel),
        _ => route_hermes_path(rel),
    }
}

/// hermes: three out-of-tree plugin families plus hooks and cron
/// scripts under `$HERMES_HOME`, and the context engine inside the
/// checkout.
fn route_hermes_path(rel: &str) -> Option<(Dest, String)> {
    // The three out-of-tree plugin families flatten into
    // `$HERMES_HOME/plugins/<name>/` — hermes's user plugin dir has no
    // family subdirectories.
    for family in ["plugins/memory/", "plugins/gateway/", "plugins/agent/"] {
        if let Some(r) = rel.strip_prefix(family) {
            return Some((Dest::HermesHome, format!("plugins/{r}")));
        }
    }
    if rel.starts_with("plugins/context_engine/") {
        return Some((Dest::HermesSrc, rel.to_owned()));
    }
    // Gateway hooks (`hooks/<name>/HOOK.yaml` + `handler.py`) and cron
    // `--script` files keep their tree under `$HERMES_HOME/`.
    if rel.starts_with("hooks/") || rel.starts_with("scripts/") {
        return Some((Dest::HermesHome, rel.to_owned()));
    }
    None
}

/// nanoclaw: two directories travel, and nothing else. The agent
/// template keeps its path; the fork skill moves under the checkout's
/// `.claude/skills/`, which is where nanoclaw looks for one. Everything
/// the skill needs to run in the fork — its modules, its patcher, its
/// tests — is inside that directory and travels with it.
fn route_nanoclaw_path(rel: &str) -> Option<(Dest, String)> {
    if rel.starts_with("templates/mwe/") {
        return Some((Dest::NanoclawDir, rel.to_owned()));
    }
    if rel.starts_with("skills/add-mwe-memory/") {
        return Some((Dest::NanoclawDir, format!(".claude/{rel}")));
    }
    None
}

/// Embedded bridge files in deterministic order, `(relpath, utf8)`.
fn bridge_files(consumer: &str) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = match consumer {
        "nanoclaw" => NanoclawBridge::iter()
            .filter_map(|rel| {
                NanoclawBridge::get(&rel).map(|f| {
                    (
                        rel.into_owned(),
                        String::from_utf8_lossy(&f.data).into_owned(),
                    )
                })
            })
            .collect(),
        // Keep Python build artifacts out of the consumer's checkout:
        // CPython writes them under `__pycache__`, so dropping that dir
        // drops every `.pyc` (the sole guard — see [`HermesBridge`]).
        _ => HermesBridge::iter()
            .filter(|rel| !rel.contains("__pycache__"))
            .filter_map(|rel| {
                HermesBridge::get(&rel).map(|f| {
                    (
                        rel.into_owned(),
                        String::from_utf8_lossy(&f.data).into_owned(),
                    )
                })
            })
            .collect(),
    };
    files.sort();
    files
}

/// Read one string key of the `[upstream]` table out of the nanoclaw
/// bridge's embedded `bridge.toml`, so the installer clones the exact
/// repo and ref the bridge is tested against and the two can never
/// drift. `None` when the key is absent — the installer then 404s
/// rather than shipping an unpinned `git clone`.
///
/// A hand-rolled scan rather than a TOML parser: this reads two string
/// keys out of a file this repository writes, and a dependency to do it
/// would be paid by every build.
fn nanoclaw_upstream(key: &str) -> Option<String> {
    let manifest = NanoclawBridge::get("bridge.toml")?;
    let manifest = String::from_utf8_lossy(&manifest.data).into_owned();
    let mut table = "";
    for line in manifest.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            table = name;
        } else if table == "upstream"
            && let Some((k, value)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Generate the self-contained POSIX installer, or `None` for a
/// consumer without one (claude-code registers itself over MCP; an
/// unknown name has no bridge at all).
fn render_install_sh(consumer: &str) -> Option<String> {
    match consumer {
        "hermes" => Some(render_install_sh_hermes()),
        "nanoclaw" => render_install_sh_nanoclaw(),
        _ => None,
    }
}

/// Append every routed file of a bridge as a `mkdir -p` + quoted
/// heredoc, so the installer carries its whole tree with no archive,
/// no `tar` and no second fetch. The delimiter is quoted, so nothing
/// inside a file is expanded by the shell.
fn push_heredoc_writes(s: &mut String, consumer: &str) {
    for (rel, content) in bridge_files(consumer) {
        let Some((dest, dest_rel)) = route_embedded_path(consumer, &rel) else {
            continue;
        };
        let full = format!("{}/{dest_rel}", dest.sh_var());
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
}

#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "shell ${VAR:-default} braces are not Rust format args"
)]
fn render_install_sh_hermes() -> String {
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

    push_heredoc_writes(&mut s, "hermes");

    s.push_str(
        "\nprintf '%s\\n' \"\" \\\n  \"mwe-mcp hermes bridge: files installed.\" \\\n  \"  memory + media + watchdog -> $HERMES_HOME/plugins/\" \\\n  \"  reverse-channel hook -> $HERMES_HOME/hooks/mwe-events/ (auto-discovered)\" \\\n  \"  daily digest script -> $HERMES_HOME/scripts/mwe-daily-digest.py\" \\\n  \"  context engine -> $HERMES_SRC/plugins/context_engine/\" \\\n  \"\" \\\n  \"Four steps remain — they are yours (the installer never handles your token):\" \\\n  \"  1. Issue a token from your mwe-mcp dashboard home and set MWE_TOKEN in hermes's .env.\" \\\n  \"  2. Disable hermes's built-in memory (memory_enabled: false AND user_profile_enabled: false) so mwe-mcp is the only memory.\" \\\n  \"  3. Enable the hook plugins in config.yaml plugins.enabled: mwe-watchdog (recommended) and mwe-media (if you want media capture).\" \\\n  \"  4. Restart hermes so it loads the new plugins.\" \\\n  \"Optional: the daily memory digest cron — see the header of mwe-daily-digest.py.\"\n",
    );
    s
}

/// The nanoclaw installer resolves a fork before it writes anything:
/// an explicit `NANOCLAW_DIR` wins, else the current directory when it
/// is already a checkout, else `~/nanoclaw` — cloned at the manifest's
/// pin when it does not exist, and refused when it exists as something
/// else, because writing a template into a stranger's directory is
/// worse than stopping.
///
/// It then leaves the fork ready for nanoclaw's own setup wizard: the
/// registry branches fetched into remote-tracking refs
/// ([`NANOCLAW_REGISTRY_BRANCHES`]) so a channel can be installed, and
/// the template pick in `.env` ([`NANOCLAW_TEMPLATE_ENV_LINE`]) so the
/// wizard offers this agent. Neither is fatal to the file placement and
/// neither goes near the token.
///
/// `None` when the manifest carries no upstream repo or pin: an
/// installer that cloned an unpinned `main` would place a bridge beside
/// a nanoclaw it was never tested against.
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "shell ${VAR:-default} braces are not Rust format args"
)]
fn render_install_sh_nanoclaw() -> Option<String> {
    let repo = nanoclaw_upstream("repo")?;
    let pin = nanoclaw_upstream("pin")?;
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str(
        "# mwe-mcp NanoClaw bridge installer — self-contained, served by your mwe-mcp server.\n",
    );
    s.push_str(
        "# Places the `mwe` agent template and the add-mwe-memory fork skill into a\n\
         # NanoClaw checkout, cloning one at the tested ref if you have none.\n\
         # It needs only git: nanoclaw.sh installs Node, pnpm and Docker itself.\n\
         # It never touches your token.\n",
    );
    s.push_str("set -eu\n\n");
    s.push_str("NANOCLAW_REPO=\"");
    s.push_str(&repo);
    s.push_str("\"\nNANOCLAW_REF=\"");
    s.push_str(&pin);
    s.push_str("\"\n\n");
    s.push_str("# A checkout is a directory carrying nanoclaw's own entry point and\n");
    s.push_str("# package name — enough to tell a fork from an unrelated directory.\n");
    s.push_str("is_nanoclaw_checkout() {\n");
    s.push_str("  [ -f \"$1/nanoclaw.sh\" ] && grep -q '\"name\": *\"nanoclaw\"' \"$1/package.json\" 2>/dev/null\n");
    s.push_str("}\n\n");
    s.push_str("if [ -n \"${NANOCLAW_DIR:-}\" ]; then\n");
    s.push_str("  :\n");
    s.push_str("elif is_nanoclaw_checkout \"$(pwd)\"; then\n");
    s.push_str("  NANOCLAW_DIR=\"$(pwd)\"\n");
    s.push_str("else\n");
    s.push_str("  NANOCLAW_DIR=\"$HOME/nanoclaw\"\n");
    s.push_str("fi\n\n");
    s.push_str("if [ -e \"$NANOCLAW_DIR\" ]; then\n");
    s.push_str("  if ! is_nanoclaw_checkout \"$NANOCLAW_DIR\"; then\n");
    s.push_str("    echo \"error: $NANOCLAW_DIR exists but is not a nanoclaw checkout.\" >&2\n");
    s.push_str("    echo \"       Set NANOCLAW_DIR=/path/to/your/nanoclaw, or move that directory aside.\" >&2\n");
    s.push_str("    exit 1\n");
    s.push_str("  fi\n");
    s.push_str("  echo \"using the nanoclaw checkout at $NANOCLAW_DIR\"\n");
    s.push_str("else\n");
    s.push_str("  command -v git >/dev/null 2>&1 || { echo \"error: git is needed to fetch nanoclaw\" >&2; exit 1; }\n");
    s.push_str("  echo \"cloning nanoclaw $NANOCLAW_REF into $NANOCLAW_DIR\"\n");
    s.push_str(
        "  git clone --branch \"$NANOCLAW_REF\" --depth 1 \"$NANOCLAW_REPO\" \"$NANOCLAW_DIR\"\n",
    );
    s.push_str("fi\n\n");

    // nanoclaw's channel and provider skills read their payloads with
    // `git show origin/<branch>:<path>`. A clone of a single ref tracks
    // only that ref, so `origin/channels` does not resolve and the setup
    // wizard dies at the channel step; name the branches in the refspec
    // and the refs exist. A fork that cannot reach them still gets the
    // bridge — the warning says what to run later.
    s.push_str("# nanoclaw copies a channel adapter out of its registry branches with\n");
    s.push_str("# `git show origin/<branch>:<path>`, so those refs have to exist here.\n");
    s.push_str("for branch in ");
    s.push_str(&NANOCLAW_REGISTRY_BRANCHES.join(" "));
    s.push_str("; do\n");
    s.push_str(
        "  git -C \"$NANOCLAW_DIR\" rev-parse --verify --quiet \"refs/remotes/origin/$branch\" >/dev/null 2>&1 && continue\n",
    );
    s.push_str("  echo \"fetching origin/$branch\"\n");
    s.push_str(
        "  git -C \"$NANOCLAW_DIR\" fetch --depth 1 --quiet origin \"$branch:refs/remotes/origin/$branch\" ||\n",
    );
    s.push_str(
        "    echo \"warning: could not fetch origin/$branch — installing a channel will fail until 'git -C $NANOCLAW_DIR fetch --depth 1 origin $branch:refs/remotes/origin/$branch' succeeds\" >&2\n",
    );
    s.push_str("done\n\n");

    push_heredoc_writes(&mut s, "nanoclaw");

    // The wizard reads this one key out of the fork's `.env` and offers
    // that template. Written only when absent, appended on its own line,
    // and every other line — the token included — is left alone.
    s.push_str("\n# nanoclaw's setup wizard reads this key from .env and offers that template.\n");
    s.push_str("NANOCLAW_ENV=\"$NANOCLAW_DIR/.env\"\n");
    s.push_str("if ! grep -q '^NANOCLAW_TEMPLATE_PATH=' \"$NANOCLAW_ENV\" 2>/dev/null; then\n");
    s.push_str("  printf '\\n%s\\n' \"");
    s.push_str(NANOCLAW_TEMPLATE_ENV_LINE);
    s.push_str("\" >> \"$NANOCLAW_ENV\"\n");
    s.push_str("  echo \"set ");
    s.push_str(NANOCLAW_TEMPLATE_ENV_LINE);
    s.push_str(" in $NANOCLAW_ENV\"\n");
    s.push_str("fi\n");

    s.push_str(
        "\nprintf '%s\\n' \"\" \\\n  \
         \"mwe-mcp NanoClaw bridge: files installed into $NANOCLAW_DIR.\" \\\n  \
         \"  mwe agent template -> $NANOCLAW_DIR/templates/mwe/\" \\\n  \
         \"  add-mwe-memory skill -> $NANOCLAW_DIR/.claude/skills/add-mwe-memory/\" \\\n  \
         \"\" \\\n  \
         \"Five steps remain — they are yours; the installer never handles your token:\" \\\n  \
         \"  1. Stamp the agent. Fresh install: run 'bash nanoclaw.sh' from $NANOCLAW_DIR and confirm the mwe template it offers. Existing install: ncl groups create --template mwe --name mwe --new (--name is yours).\" \\\n  \
         \"  2. Apply the skill: /add-mwe-memory from Claude Code, or the shell commands listed in .claude/skills/add-mwe-memory/SKILL.md.\" \\\n  \
         \"  3. Issue a STANDARD consumer token from your mwe-mcp dashboard, set MWE_TOKEN in $NANOCLAW_DIR/.env, and tick every person it speaks for plus guest in its delegations.\" \\\n  \
         \"  4. Fill in senderMap in $NANOCLAW_DIR/mwe.json — one line per person — and restart nanoclaw.\" \\\n  \
         \"  5. Connect a channel and talk to it.\"\n",
    );
    Some(s)
}

/// Generate the self-contained PowerShell installer — hermes only.
///
/// nanoclaw runs on Windows inside WSL2, which is a Linux install: its
/// Windows path is the same `install.sh` run from a WSL2 shell, and the
/// guide says so. A PowerShell script would place a fork on the Windows
/// side that `nanoclaw.sh` cannot be run from, so this bridge ships
/// none.
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

    for (rel, content) in bridge_files("hermes") {
        let Some((dest, dest_rel)) = route_embedded_path("hermes", &rel) else {
            continue;
        };
        let base_var = match dest {
            Dest::HermesHome => "$HermesHome",
            Dest::HermesSrc => "$HermesSrc",
            // This installer is hermes-only (see the fn doc), so the
            // nanoclaw destination can never be routed into it.
            Dest::NanoclawDir => continue,
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
    // Same four destinations the shell installer prints. The context-engine
    // plugin is the one that does not land under HERMES_HOME, so a Windows
    // operator who is not told where it went cannot check that it arrived.
    s.push_str("Write-Host \"mwe-mcp hermes bridge: files installed.\"\n");
    s.push_str("Write-Host \"  memory + media + watchdog -> $HermesHome\\plugins\\\"\n");
    s.push_str("Write-Host \"  reverse-channel hook -> $HermesHome\\hooks\\mwe-events\\ (auto-discovered)\"\n");
    s.push_str(
        "Write-Host \"  daily digest script -> $HermesHome\\scripts\\mwe-daily-digest.py\"\n",
    );
    s.push_str("Write-Host \"  context engine -> $HermesSrc\\plugins\\context_engine\\\"\n");
    s.push_str("Write-Host \"\"\n");
    s.push_str("Write-Host \"Four steps remain — they are yours (the installer never handles your token):\"\n");
    s.push_str("Write-Host \"  1. Issue a token from your mwe-mcp dashboard home and set MWE_TOKEN in hermes's .env.\"\n");
    s.push_str("Write-Host \"  2. Disable hermes's built-in memory (memory_enabled: false and user_profile_enabled: false).\"\n");
    s.push_str("Write-Host \"  3. Enable the hook plugins in config.yaml plugins.enabled: mwe-watchdog (recommended) and mwe-media (for media capture).\"\n");
    s.push_str("Write-Host \"  4. Restart hermes so it loads the new plugins.\"\n");
    s.push_str("Write-Host \"Optional: the daily memory digest cron — see the header of mwe-daily-digest.py.\"\n");
    Some(s)
}

/// Machine-readable instructions an agent is pointed at ("Read … and
/// follow"). `origin` is the request-derived public origin. Dispatches
/// per consumer; `None` for one without a served `install.md`.
fn render_install_md(consumer: &str, origin: &str) -> Option<String> {
    match consumer {
        "nanoclaw" => Some(render_install_md_nanoclaw(origin)),
        "hermes" => Some(render_install_md_hermes(origin)),
        "claude-code" => Some(render_install_md_claude_code(origin)),
        _ => None,
    }
}

/// Agent-driven install for the **nanoclaw** bridge — points the agent
/// at the served `curl … | sh` installer, then hands it the five steps
/// it must have the *operator* do. The token is one of them: an agent
/// that mints or pastes a credential on the operator's behalf is the
/// one failure mode this whole surface is shaped to prevent.
fn render_install_md_nanoclaw(origin: &str) -> String {
    format!(
        "# Install the mwe-mcp NanoClaw bridge\n\
         \n\
         You are connecting **NanoClaw** — the ready-made assistant of a\n\
         first-party mwe-mcp memory server at `{origin}` — to that server. The\n\
         installer is served by the same server:\n\
         \n\
         ```sh\n\
         curl -fsSL {origin}/bridges/nanoclaw/install.sh | sh\n\
         ```\n\
         \n\
         Run it from anywhere. If the current directory is already a NanoClaw\n\
         checkout it uses that one; otherwise it clones NanoClaw at the tested\n\
         ref into `~/nanoclaw`. Set `NANOCLAW_DIR` to install into a fork that\n\
         lives elsewhere. If that path exists and is *not* a NanoClaw checkout\n\
         the installer stops instead of writing into it — do not work around\n\
         that, ask the operator which fork they mean.\n\
         \n\
         There is **no PowerShell installer**: NanoClaw runs on Windows inside\n\
         WSL2, so on Windows the operator runs the same command in a WSL2 shell.\n\
         \n\
         The installer needs only `git`; NanoClaw's own `nanoclaw.sh` installs\n\
         Node, pnpm and Docker if they are missing. It places two directories —\n\
         the `mwe` agent template in `templates/mwe/` and the `add-mwe-memory`\n\
         fork skill in `.claude/skills/add-mwe-memory/` — fetches NanoClaw's\n\
         `channels` and `providers` branches (its channel adapters are copied\n\
         out of them, so a channel cannot be installed without them), names\n\
         `mwe` as the template in the checkout's `.env`, and **does not touch\n\
         the token**.\n\
         \n\
         Once the files are in place, **tell your operator** to do these five\n\
         things. Do not attempt them silently, and do not handle the token\n\
         yourself:\n\
         \n\
         1. Stamp the agent. On a fresh install, run `bash nanoclaw.sh` from the\n\
            checkout and confirm the `mwe` template setup offers (the installer\n\
            named it in `.env`); declining drops back to picking **Local\n\
            templates**, then `mwe`, by hand. On an install that already has\n\
            agents: `ncl groups create --template mwe --name mwe --new` —\n\
            `--name` is theirs to choose and becomes the group folder. It is\n\
            **not** what the agent answers to: its name is a fact of this\n\
            memory, and anybody it serves can tell it in chat.\n\
         2. Apply the skill: `/add-mwe-memory` from Claude Code. Without Claude\n\
            Code, the same steps are ordinary shell commands in\n\
            `.claude/skills/add-mwe-memory/SKILL.md`. It asks three questions —\n\
            the endpoint, their chat id, their mwe user id — then copies the\n\
            modules in, splices the reach-ins into NanoClaw's own files, writes\n\
            `mwe.json`, builds and tests, and restarts both the service **and**\n\
            the agent containers. Do not skip the second restart: a container\n\
            that keeps running answers with the code as it was before the\n\
            skill, with nothing in any log to say so.\n\
         3. Issue a **standard** consumer token from the mwe-mcp dashboard and\n\
            set it as `MWE_TOKEN` in the checkout's `.env`, then tick every\n\
            person the agent will speak for — plus `guest` — in that consumer's\n\
            delegations. Without the `guest` delegation an unrecognised sender\n\
            is refused rather than answered anonymously.\n\
         4. Fill in `senderMap` in `mwe.json`, one line per person\n\
            (`<channel>:<platform id>` → their mwe user id), and restart\n\
            NanoClaw. Anyone not listed speaks as a guest; there is no fallback\n\
            to the owner.\n\
         5. Connect a channel (`/manage-channels`, or `ncl wirings create`) and\n\
            talk to the agent.\n\
         \n\
         The `mwe` template is what switches NanoClaw's built-in memory off for\n\
         a group: a group carrying that plugin creates no `memory/` tree, injects\n\
         nothing at session start, and carries no session between turns, so\n\
         mwe-mcp is its only memory. A group without it is an ordinary NanoClaw\n\
         agent, untouched.\n"
    )
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
         The install ends here. Later, inside a project, pass the derived `project_id`\n\
         to `smart_bootstrap` (the `core` skill carries the exact recipe): the response\n\
         says whether that project already has a wiki, and when it has none it points\n\
         you at the `smart-onboarding` skill — which is where the whole first-connect\n\
         procedure lives, including the rules about **not** opening it mid-task and\n\
         never asking twice. Nothing is ever scanned or copied without the user's yes,\n\
         and the local copy is left exactly as it is.\n\
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
         That places the four plugins, the `mwe-events` reverse-channel gateway\n\
         hook (auto-discovered from `~/.hermes/hooks/` — it delivers\n\
         `fact_minted_for_you` and `reminder_due` notices to their recipients),\n\
         and the daily-digest\n\
         cron script. It does **not** touch the token. Once the\n\
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
         4. Restart hermes so it loads the new plugins (the gateway hook\n\
            needs no enabling — the hooks directory is discovered on start).\n\
         \n\
         Optional fifth, for a once-a-day memory recap: the cron command in the\n\
         header of `~/.hermes/scripts/mwe-daily-digest.py`.\n"
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
        &catalog_body("", &origin_from_host(&host), /* may_mint */ false),
    ))
}

async fn public_bridge_page(Path(consumer): Path<String>, Host(host): Host) -> Response {
    bridge_label(&consumer).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |label| {
            Html(layout::anonymous_reading_page(
                &format!("{label} bridge"),
                &guide_body(
                    &consumer,
                    &origin_from_host(&host),
                    /* may_mint */ false,
                ),
            ))
            .into_response()
        },
    )
}

// --- dashboard tab (authenticated shell, links resolve under /dashboard) ---

async fn tab_bridges_index(
    State(state): State<DashboardState>,
    user: SessionUser,
    Host(host): Host,
) -> Html<String> {
    let chrome = layout::Chrome::of(&state);
    Html(layout::authenticated_page(
        chrome,
        "Bridges",
        &user,
        &catalog_body("/dashboard", &origin_from_host(&host), user.is_admin),
    ))
}

async fn tab_bridge_page(
    State(state): State<DashboardState>,
    Path(consumer): Path<String>,
    Host(host): Host,
    user: SessionUser,
) -> Response {
    let chrome = layout::Chrome::of(&state);
    bridge_label(&consumer).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |label| {
            Html(layout::authenticated_page(
                chrome,
                &format!("{label} bridge"),
                &user,
                &guide_body(&consumer, &origin_from_host(&host), user.is_admin),
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
        for (rel, _) in bridge_files("hermes") {
            assert!(
                !rel.contains("__pycache__"),
                "embedded a __pycache__ path: {rel}"
            );
        }
        let rels: Vec<String> = bridge_files("hermes").into_iter().map(|(r, _)| r).collect();
        assert!(
            rels.iter().any(|r| r.starts_with("plugins/memory/mwe/")),
            "plugins/memory/mwe missing: {rels:?}"
        );
        assert!(
            rels.iter()
                .any(|r| r.starts_with("plugins/gateway/mwe-media/")),
            "plugins/gateway/mwe-media missing"
        );
        assert!(
            rels.iter().any(|r| r.starts_with("hooks/mwe-events/")),
            "hooks/mwe-events missing"
        );
        assert!(
            rels.iter().any(|r| r == "scripts/mwe-daily-digest.py"),
            "scripts/mwe-daily-digest.py missing"
        );
        assert!(
            rels.iter()
                .any(|r| r.starts_with("plugins/context_engine/mwe-truncate/")),
            "plugins/context_engine/mwe-truncate missing"
        );
        assert!(
            rels.iter()
                .any(|r| r.starts_with("plugins/agent/mwe-watchdog/")),
            "plugins/agent/mwe-watchdog missing"
        );
    }

    #[test]
    fn delimiter_never_collides_with_bridge_source() {
        for consumer in ["hermes", "nanoclaw"] {
            for (rel, content) in bridge_files(consumer) {
                // Only routed files travel inside heredocs/here-strings;
                // README/smokes/manifest never reach an installer.
                if route_embedded_path(consumer, &rel).is_none() {
                    continue;
                }
                for line in content.lines() {
                    assert_ne!(
                        line, SH_DELIM,
                        "{consumer}/{rel}: a line equals the sh heredoc delimiter"
                    );
                    assert!(
                        !line.starts_with("'@"),
                        "{consumer}/{rel}: a line starts with the PowerShell here-string terminator"
                    );
                }
            }
        }
    }

    #[test]
    fn nanoclaw_embed_carries_the_two_directories_that_travel() {
        let rels: Vec<String> = bridge_files("nanoclaw")
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert!(
            rels.iter().any(|r| r == "templates/mwe/plugin.json"),
            "the mwe template is missing: {rels:?}"
        );
        assert!(
            rels.iter()
                .any(|r| r == "templates/mwe/ai.nanoco.nanoclaw/context/instructions.md"),
            "the template persona is missing"
        );
        assert!(
            rels.iter().any(|r| r == "skills/add-mwe-memory/SKILL.md"),
            "the fork skill is missing"
        );
        assert!(
            rels.iter()
                .any(|r| r.starts_with("skills/add-mwe-memory/host/")),
            "the skill's host modules are missing"
        );
        assert!(
            rels.iter()
                .any(|r| r.starts_with("skills/add-mwe-memory/container/")),
            "the skill's container modules are missing"
        );
    }

    /// Two directories travel to a fork and nothing else: the bridge's
    /// own README, manifest and smokes stay on the server. The skill's
    /// tests DO travel — `SKILL.md` copies them into the fork, so an
    /// upgrade that moves a reach-in fails there.
    #[test]
    fn nanoclaw_routing_keeps_the_bridge_scaffolding_at_home() {
        for (rel, _) in bridge_files("nanoclaw") {
            let routed = route_embedded_path("nanoclaw", &rel);
            let travels =
                rel.starts_with("templates/mwe/") || rel.starts_with("skills/add-mwe-memory/");
            assert_eq!(
                routed.is_some(),
                travels,
                "{rel}: routed={} but travels={travels}",
                routed.is_some()
            );
        }
        assert!(route_embedded_path("nanoclaw", "README.md").is_none());
        assert!(route_embedded_path("nanoclaw", "bridge.toml").is_none());
        assert!(route_embedded_path("nanoclaw", "smoke.sh").is_none());
        assert!(route_embedded_path("nanoclaw", "smoke_test.ts").is_none());
        assert!(route_embedded_path("nanoclaw", "stub_runner.py").is_none());
    }

    #[test]
    fn nanoclaw_install_sh_writes_two_trees_and_leaves_the_token_alone() {
        let sh = render_install_sh("nanoclaw").expect("nanoclaw sh");
        assert!(sh.starts_with("#!/bin/sh"));
        assert!(sh.contains("cat > \"$NANOCLAW_DIR/templates/mwe/plugin.json\""));
        assert!(sh.contains(
            "cat > \"$NANOCLAW_DIR/templates/mwe/ai.nanoco.nanoclaw/context/instructions.md\""
        ));
        assert!(sh.contains("cat > \"$NANOCLAW_DIR/.claude/skills/add-mwe-memory/SKILL.md\""));
        assert!(
            sh.contains("cat > \"$NANOCLAW_DIR/.claude/skills/add-mwe-memory/host/client.ts\"")
        );
        // The bridge's own scaffolding never reaches the fork.
        assert!(
            !sh.contains("$NANOCLAW_DIR/README.md")
                && !sh.contains("$NANOCLAW_DIR/bridge.toml")
                && !sh.contains("$NANOCLAW_DIR/smoke.sh")
                && !sh.contains("$NANOCLAW_DIR/stub_runner.py"),
            "non-runtime bridge files must not ride the installer"
        );
        // Fork resolution: an explicit dir, else the cwd when it is a
        // checkout, else a clone into the default.
        assert!(sh.contains("if [ -n \"${NANOCLAW_DIR:-}\" ]"));
        assert!(sh.contains("NANOCLAW_DIR=\"$(pwd)\""));
        assert!(sh.contains("NANOCLAW_DIR=\"$HOME/nanoclaw\""));
        assert!(sh.contains("is not a nanoclaw checkout"));
        assert!(sh.contains("git clone --branch \"$NANOCLAW_REF\" --depth 1"));
        // The residual steps, and no token anywhere near them.
        assert!(sh.contains("Five steps remain — they are yours"));
        assert!(sh.contains("ncl groups create --template mwe --name mwe --new"));
        assert!(sh.contains("confirm the mwe template it offers"));
        assert!(sh.contains("/add-mwe-memory"));
        assert!(sh.contains("STANDARD consumer token"));
        assert!(sh.contains("senderMap"));
        assert!(sh.contains("Connect a channel"));
    }

    /// The installer clones the ref the bridge is tested against. The
    /// pin is written once, in `bridge.toml`; this reads that file
    /// straight off disk so a bumped manifest and a stale installer
    /// cannot both be green.
    #[test]
    fn nanoclaw_install_sh_carries_the_manifest_pin() {
        let manifest = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../agents-bridges/nanoclaw/bridge.toml"
        ));
        let value_of = |key: &str| -> String {
            let (_, value) = manifest
                .lines()
                .map(str::trim)
                .find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == key))
                .unwrap_or_else(|| panic!("bridge.toml has no {key}"));
            value.trim().trim_matches('"').to_owned()
        };
        let pin = value_of("pin");
        let repo = value_of("repo");
        assert_eq!(nanoclaw_upstream("pin").as_deref(), Some(pin.as_str()));
        let sh = render_install_sh("nanoclaw").expect("nanoclaw sh");
        assert!(
            sh.contains(&format!("NANOCLAW_REF=\"{pin}\"")),
            "the installer does not carry the manifest pin {pin}"
        );
        assert!(sh.contains(&format!("NANOCLAW_REPO=\"{repo}\"")));
    }

    /// A clone of one ref tracks one ref, and nanoclaw's channel and
    /// provider skills read their payloads out of two branches with `git
    /// show origin/<branch>:<path>`. Without those remote-tracking refs
    /// the setup wizard dies at the channel step on `fatal: invalid
    /// object name 'origin/channels'`, so the installer fetches them —
    /// for a fork it just cloned and for one it was pointed at.
    #[test]
    fn nanoclaw_install_sh_brings_the_branches_the_channel_skills_read() {
        let sh = render_install_sh("nanoclaw").expect("nanoclaw sh");
        assert_eq!(
            NANOCLAW_REGISTRY_BRANCHES,
            ["channels", "providers"],
            "the branches nanoclaw's skills name"
        );
        assert!(
            sh.contains("for branch in channels providers; do"),
            "both branches must be fetched, not just the one a channel needs"
        );
        // An explicit refspec: a bare `git fetch origin channels` in a
        // single-ref clone writes FETCH_HEAD and no `origin/channels`,
        // which is exactly the failure this fixes.
        assert!(sh.contains(
            "git -C \"$NANOCLAW_DIR\" fetch --depth 1 --quiet origin \"$branch:refs/remotes/origin/$branch\""
        ));
        // Already there → left alone; unreachable → a warning, never a
        // dead install: the two directories are the job, the branches are
        // the wizard's.
        assert!(sh.contains("rev-parse --verify --quiet \"refs/remotes/origin/$branch\""));
        assert!(sh.contains("warning: could not fetch origin/$branch"));
        // It runs after the fork is resolved, so a reused checkout gets
        // them too — not only a fresh clone.
        let fetch = sh.find("for branch in channels").expect("the fetch loop");
        let resolved = sh
            .find("using the nanoclaw checkout at")
            .expect("fork resolution");
        assert!(
            fetch > resolved,
            "the fetch must cover a reused checkout as well"
        );
    }

    /// The one setup key nanoclaw reads out of a fork's `.env`
    /// (`setup/auto.ts` bridges `NANOCLAW_TEMPLATE_PATH` into the run;
    /// nothing loads the rest of the file into the environment). Written
    /// only when absent, on its own line, and never near the token.
    #[test]
    fn nanoclaw_install_sh_names_the_template_and_touches_nothing_else_in_env() {
        let sh = render_install_sh("nanoclaw").expect("nanoclaw sh");
        assert_eq!(NANOCLAW_TEMPLATE_ENV_LINE, "NANOCLAW_TEMPLATE_PATH=mwe");
        assert!(sh.contains("NANOCLAW_ENV=\"$NANOCLAW_DIR/.env\""));
        assert!(
            sh.contains(
                "if ! grep -q '^NANOCLAW_TEMPLATE_PATH=' \"$NANOCLAW_ENV\" 2>/dev/null; then"
            ),
            "an operator's own pick must survive"
        );
        assert!(
            sh.contains("printf '\\n%s\\n' \"NANOCLAW_TEMPLATE_PATH=mwe\" >> \"$NANOCLAW_ENV\"")
        );
        // Only that key. The other setup env vars are read from the
        // process environment, never from `.env`, so writing them here
        // would be a line that looks like configuration and does nothing.
        assert!(!sh.contains("NANOCLAW_AGENT_NAME"));
        assert!(!sh.contains("NANOCLAW_AGENT_PROVIDER"));
        assert!(!sh.contains("NANOCLAW_DISPLAY_NAME"));
        // Exactly one line of the script writes to that file, and it is
        // the template pick. The token appears in the installer only
        // inside the skill's own operator instructions, never as
        // something this script writes.
        assert_eq!(
            sh.matches(">> \"$NANOCLAW_ENV\"").count(),
            1,
            "the installer must append exactly one line to the fork's .env"
        );
        assert!(!sh.contains("MWE_TOKEN=$"));
    }

    /// nanoclaw runs on Windows only inside WSL2, so the honest Windows
    /// path is the same `sh` command in a WSL2 shell — not a PowerShell
    /// installer that would place a fork bash cannot run from.
    #[test]
    fn nanoclaw_has_no_powershell_installer_and_the_guide_says_why() {
        assert!(render_install_ps1("nanoclaw").is_none());
        let html = guide_body(
            "nanoclaw",
            "https://memory.anna.dev",
            /* may_mint */ true,
        )
        .into_string();
        assert!(html.contains("WSL2"));
        assert!(!html.contains("install.ps1"));
        let md = render_install_md("nanoclaw", "https://memory.anna.dev").expect("nanoclaw md");
        assert!(md.contains("WSL2"));
        assert!(!md.contains("install.ps1"));
    }

    #[test]
    fn nanoclaw_install_md_carries_origin_and_residual_steps() {
        let md = render_install_md("nanoclaw", "https://memory.anna.dev").expect("nanoclaw md");
        assert!(md.contains("curl -fsSL https://memory.anna.dev/bridges/nanoclaw/install.sh | sh"));
        assert!(md.contains("tell your operator"));
        assert!(md.contains("MWE_TOKEN"));
        assert!(md.contains("do not handle the token"));
        assert!(md.contains("ncl groups create --template mwe --name mwe --new"));
        assert!(md.contains("/add-mwe-memory"));
        assert!(md.contains("senderMap"));
        assert!(md.contains("guest"));
        assert!(md.contains("NANOCLAW_DIR"));
    }

    #[test]
    fn nanoclaw_guide_leads_the_catalog_and_mints_no_token() {
        // nanoclaw is the first entry: the ready-made assistant is what
        // an operator with no agent of their own should reach for.
        assert_eq!(BRIDGES[0].0, "nanoclaw");
        assert_eq!(bridge_label("nanoclaw"), Some("NanoClaw (nanoco)"));

        let html = catalog_body("", "https://memory.anna.dev", /* may_mint */ true).into_string();
        let nano = html
            .find("/bridges/nanoclaw")
            .expect("nanoclaw in the catalog");
        let hermes = html.find("/bridges/hermes").expect("hermes in the catalog");
        assert!(nano < hermes, "nanoclaw must be listed before hermes");
        assert!(html.contains("ready-made assistant"));
        assert!(html.contains("/bridges/nanoclaw/install.md"));
        // Under the dashboard the guide link is prefixed, the install.md is not.
        let tab_html = catalog_body(
            "/dashboard",
            "https://memory.anna.dev",
            /* may_mint */ true,
        )
        .into_string();
        assert!(tab_html.contains("href=\"/dashboard/bridges/nanoclaw\""));
        assert!(tab_html.contains("/bridges/nanoclaw/install.md"));

        let guide = guide_body(
            "nanoclaw",
            "https://memory.anna.dev",
            /* may_mint */ true,
        )
        .into_string();
        assert!(
            guide.contains("https://memory.anna.dev/bridges/nanoclaw/install.sh | sh"),
            "the guide must show the served install command"
        );
        assert!(guide.contains("confirm the "));
        assert!(guide.contains("channels"));
        assert!(guide.contains("MWE_TOKEN"));
        // The token is issued from the Tokens page, never minted here.
        assert!(guide.contains("href=\"/dashboard/tokens\""));
    }

    #[test]
    fn install_sh_is_self_contained_and_routes_destinations() {
        let sh = render_install_sh("hermes").expect("hermes sh");
        assert!(sh.contains("cat > \"$HERMES_HOME/plugins/mwe/"));
        assert!(sh.contains("cat > \"$HERMES_HOME/plugins/mwe-media/"));
        assert!(sh.contains("cat > \"$HERMES_HOME/plugins/mwe-watchdog/"));
        assert!(sh.contains("cat > \"$HERMES_HOME/hooks/mwe-events/handler.py"));
        assert!(sh.contains("cat > \"$HERMES_HOME/hooks/mwe-events/HOOK.yaml"));
        assert!(sh.contains("cat > \"$HERMES_HOME/scripts/mwe-daily-digest.py"));
        assert!(sh.contains("cat > \"$HERMES_SRC/plugins/context_engine/mwe-truncate/"));
        assert!(
            !sh.contains("README.md") && !sh.contains("smoke_test.py"),
            "non-runtime bridge files must not ride the installer"
        );
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
        assert!(ps.contains("\"plugins/context_engine/mwe-truncate/"));
        assert!(ps.contains("\"hooks/mwe-events/handler.py\""));
        assert!(ps.contains("\"scripts/mwe-daily-digest.py\""));
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
        assert!(html.contains("If you are a consumer"));
        assert!(html.contains("/bridges"));
        assert!(html.contains("href=\"/dashboard/\""));
    }

    #[test]
    fn catalog_lists_hermes_with_agent_instructions_link() {
        let pub_html =
            catalog_body("", "https://memory.anna.dev", /* may_mint */ true).into_string();
        assert!(pub_html.contains("hermes"));
        assert!(pub_html.contains("instructions it can follow"));
        assert!(pub_html.contains("/bridges/hermes/install.md"));
        assert!(pub_html.contains("href=\"/bridges/hermes\""));
        // The bridge-less claude.ai section shows the MCP URL to paste + the
        // skill-upload funnel.
        assert!(pub_html.contains("Connect the claude.ai web app"));
        assert!(pub_html.contains("https://memory.anna.dev/mcp"));
        assert!(pub_html.contains("/webagentoauth/skill.md"));
        // Under the dashboard the guide link is prefixed, the install.md is not.
        let tab_html = catalog_body(
            "/dashboard",
            "https://memory.anna.dev",
            /* may_mint */ true,
        )
        .into_string();
        assert!(tab_html.contains("href=\"/dashboard/bridges/hermes\""));
        assert!(tab_html.contains("/bridges/hermes/install.md"));
    }

    #[test]
    fn guide_has_install_command_and_no_inline_token_mint() {
        let html = guide_body(
            "hermes",
            "https://memory.anna.dev",
            /* may_mint */ true,
        )
        .into_string();
        assert!(html.contains("https://memory.anna.dev/bridges/hermes/install.sh"));
        assert!(html.contains("install.md"));
        assert!(html.contains("memory_enabled: false"));
        assert!(html.contains("user_profile_enabled: false"));
        assert!(html.contains("mwe-watchdog"));
        assert!(html.contains("Restart hermes"));
        // The token is issued on the Tokens page, not minted here — and it
        // is a standard one, because hermes is a standard consumer. The
        // nanoclaw guide says the same, in the same words.
        assert!(html.contains("href=\"/dashboard/tokens\""), "{html}");
        assert!(
            html.contains("<strong>standard</strong> consumer token"),
            "{html}"
        );
        assert!(!html.contains("dashboard home"), "{html}");
    }

    #[test]
    fn claude_code_guide_is_smart_and_has_no_curl_installer() {
        let html = guide_body(
            "claude-code",
            "https://memory.anna.dev",
            /* may_mint */ true,
        )
        .into_string();
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
        let html = catalog_body("", "https://memory.anna.dev", /* may_mint */ true).into_string();
        assert!(html.contains("Claude Code (Anthropic)"));
        assert!(html.contains("href=\"/bridges/claude-code\""));
        assert!(html.contains("/bridges/claude-code/install.md"));
    }

    #[tokio::test]
    async fn public_routes_serve_pages_and_scripts() {
        let cases = [
            ("/", "If you are a consumer"),
            ("/bridges", "instructions it can follow"),
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
            // nanoclaw is a known consumer that ships no PowerShell
            // installer: the Windows path is the sh command in WSL2.
            "/bridges/nanoclaw/install.ps1",
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

    /// The public guide, the installer and the agent instructions all
    /// answer for nanoclaw. The authenticated tab is gated by the same
    /// [`bridge_label`] lookup, so a label is what decides 200 vs 404
    /// there too.
    #[tokio::test]
    async fn nanoclaw_public_endpoints_serve() {
        for (uri, needle) in [
            ("/bridges/nanoclaw", "ready-made assistant"),
            ("/bridges/nanoclaw/install.sh", "#!/bin/sh"),
            ("/bridges/nanoclaw/install.md", "NanoClaw"),
        ] {
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
        assert!(bridge_label("nanoclaw").is_some(), "the tab needs a label");
    }

    /// The Tokens page is admin-only, so the guide links to it for
    /// whoever may open it and names it in plain words for everybody
    /// else — a reader following that link would meet a 403.
    #[test]
    fn the_tokens_page_is_a_link_only_where_it_opens() {
        let for_admin = guide_body("nanoclaw", "https://memory.anna.dev", true).into_string();
        assert!(
            for_admin.contains("href=\"/dashboard/tokens\""),
            "{for_admin}"
        );

        let for_reader = guide_body("nanoclaw", "https://memory.anna.dev", false).into_string();
        assert!(
            !for_reader.contains("href=\"/dashboard/tokens\""),
            "a reader must not be pointed at a console that refuses them: {for_reader}"
        );
        assert!(
            for_reader.contains("Tokens</strong> page of this dashboard (an admin\'s)"),
            "and must still be told where the token comes from: {for_reader}"
        );
    }
}
