// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bridge **onboarding + distribution** surface.
//!
//! The catalog and the guides are non-secret and reachable two ways, so
//! we never have to ask "who is visiting". The one exception is the
//! install claim below: it is minted only where a session says who is
//! asking, and it is the only thing this surface hands out that a person
//! must not share.
//!
//! - **Public** (root-mounted, anonymous) for consumers and `curl`:
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
//!     nanoclaw's takes `?claim=<code>`, which it carries into the script.
//!   - `POST /bridges/nanoclaw/claim` — where that script trades the code
//!     for a consumer token. The one route here that touches the database,
//!     which is why it is [`claim_router`] and not the stateless one.
//! - **Dashboard tab** (`/dashboard/bridges`, authenticated) for the
//!   operator: the *same* catalog + guide bodies wrapped in the dashboard
//!   shell, plus `POST /bridges/nanoclaw/command`, where an admin mints a
//!   claim and gets the command that carries it. Wiring a consumer in is
//!   the operator's job, so "Bridges" is a nav entry for an admin; the
//!   page answers anybody who has the address, and only an admin is
//!   offered the button. Shared body functions take a base prefix so the
//!   in-page links resolve under `/dashboard` there and at the root
//!   publicly.
//!
//! A **token** is a credential and is minted on the dashboard's Tokens
//! page. The nanoclaw installer is the one path that carries one without
//! anybody copying it, and it does so through an **install claim**: the
//! admin mints a short-lived, single-use code on the Bridges tab, the
//! served command carries the code, and the installer trades it at
//! `POST /bridges/nanoclaw/claim` for a standard consumer token it writes
//! straight into the fork's `.env`. The code is not the token — it expires,
//! it is burned on first use through the same `jti` blacklist as a
//! single-use dashboard link, and it only ever answers a request that
//! reaches this server. Every other page and script instructs the operator
//! to mint the token themselves.

use std::time::Duration;

use axum::Router;
use axum::extract::{Host, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use maud::{Markup, html};
use mwe_core::enrollment;
use mwe_core::jwt::{self, TokenClaims};
use rust_embed::RustEmbed;
use serde::Deserialize;

use crate::auth::{AdminUser, SessionUser};
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

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
/// [`route_embedded_path`] returning `None`.
///
/// Two of the unrouted files are still read here rather than shipped:
/// the manifest, because [`nanoclaw_upstream`] takes the tested repo and
/// ref out of it so the installer cannot drift from the bridge, and
/// `install-assistant.sh`, which [`nanoclaw_install_driver`] appends to
/// the served installer. That one drives a fork from outside it, so it
/// belongs to the server, not to the checkout.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../agents-bridges/nanoclaw/"]
struct NanoclawBridge;

/// Catalog of bridged consumers, in display order. A consumer appears
/// here only when its bridge ships a served onboarding surface — a
/// `curl … | sh` installer (nanoclaw, hermes) **or** an `install.md`
/// the consumer follows itself (claude-code).
///
/// nanoclaw leads: it is the ready-made assistant, the one consumer an
/// operator with none of their own can install and talk to.
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

/// The consumer id an install claim mints a standard token for. Fixed,
/// because a memory server has one ready-made assistant: a second install
/// refreshes that consumer's delegations instead of coining a second
/// identity and a second wiki nobody asked for.
const NANOCLAW_CONSUMER_ID: &str = "mwe";

/// How long an install claim stays good. The installer trades it in its
/// first seconds — before the clone, before nanoclaw's own setup, before
/// the browser sign-in — so this window covers copying the command and
/// starting it, not the install it starts.
const CLAIM_TTL: Duration = Duration::from_secs(15 * 60);

/// `device_label` of an install claim, checked before one is burned. It
/// is what stops a claim being redeemed as a browser session, and a
/// session cookie or an MCP bearer being redeemed as a claim — the same
/// defence [`crate::routes::auth_link`] applies to a single-use link.
const CLAIM_DEVICE_LABEL: &str = "nanoclaw-install-claim";

/// Rate-limit profile an install claim carries. It never reaches `/mcp`
/// under its own name; the profile is there because the claim shape
/// requires one.
const CLAIM_RATE_LIMIT_ID: &str = "dashboard";

/// Whether a claim is shaped like one this server mints.
///
/// The claim is interpolated into a shell script the operator pipes into
/// `sh`, so the answer decides whether that script is safe to serve. A
/// JWT's alphabet is `[A-Za-z0-9_-]` in three dot-separated parts and
/// nothing else: a claim carrying anything outside it is refused rather
/// than quoted, because a claim that needs quoting is not a claim.
fn claim_is_wellformed(claim: &str) -> bool {
    !claim.is_empty()
        && claim.len() <= 4096
        && claim
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

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
/// wrapped in the dashboard shell (top nav) so it reads as a tab — plus
/// the one thing only an admin may do here: mint the install claim that
/// lets the nanoclaw installer collect its own token.
pub fn dashboard_tab_router() -> Router<DashboardState> {
    Router::new()
        .route("/bridges", get(tab_bridges_index))
        .route("/bridges/nanoclaw/command", post(tab_mint_install_command))
        .route("/bridges/:consumer", get(tab_bridge_page))
}

/// Public, anonymous **claim-redemption** route, mounted at the root of
/// the HTTP tree by `mwe-mcp-server` beside [`public_site_router`].
///
/// Separate from that router because it is the one bridge endpoint that
/// touches the database: it burns a claim and mints a consumer token.
/// Anonymous by necessity — the box running `curl … | sh` has no
/// dashboard session, and the claim in its hand is the whole credential.
pub fn claim_router(state: DashboardState) -> Router {
    Router::new()
        .route("/bridges/nanoclaw/claim", post(redeem_install_claim))
        .with_state(state)
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

/// A minted install command, rendered once on the Bridges tab.
///
/// Held apart from the claim it carries so nothing but the command line
/// itself ever reaches a template: the claim is inside that string, and
/// the string is shown to the admin who minted it and nobody else.
struct InstallCommand {
    /// The whole `curl … | sh` line, claim included.
    command: String,
    /// The instant the claim stops working, in UTC.
    expires_utc: String,
}

/// Per-consumer install guide body. Dispatches on the consumer: nanoclaw
/// and hermes ship a `curl … | sh` installer; claude-code gets an
/// `install.md` it follows itself (no files, no shell installer).
///
/// `minted` is the install command an admin has just asked for, and only
/// the nanoclaw guide has anywhere to put one. Everywhere else the token
/// is a step the reader takes on the Tokens page.
fn guide_body(
    consumer: &str,
    origin: &str,
    may_mint: bool,
    minted: Option<&InstallCommand>,
) -> Markup {
    match consumer {
        "nanoclaw" => nanoclaw_guide_body(origin, may_mint, minted),
        "claude-code" => claude_code_guide_body(origin),
        _ => hermes_guide_body(consumer, origin, may_mint),
    }
}

/// The command itself, and how the reader gets one that carries a claim.
///
/// Three readers, one block: an admin who has just minted (the command is
/// on screen, with what the claim buys and when it dies), an admin who has
/// not (the plain command, and the button), and everybody else (the plain
/// command, and where the other kind comes from).
fn nanoclaw_command_block(curl: &str, may_mint: bool, minted: Option<&InstallCommand>) -> Markup {
    html! {
        h2 { "One command" }

        @if let Some(cmd) = minted {
            (components::flash(
                "success",
                "Your install command is below. Copy it now — the claim inside it works once.",
            ))
            pre.endpoint-display { (cmd.command) }
            p.muted {
                "The claim stops working at " strong { (cmd.expires_utc) } " UTC. The "
                "installer trades it, in its first seconds, for a "
                strong { "standard" } " consumer token for "
                code { (NANOCLAW_CONSUMER_ID) } " and writes it into the fork's "
                code { ".env" } ": it is never shown on this page, never printed by "
                "the installer, and never put on a command line. The token starts "
                "out able to speak for everybody enrolled here, plus "
                code { "guest" } " — open the "
                (tokens_page(may_mint))
                " and untick whoever the assistant has no business speaking for."
            }
        } @else {
            pre.endpoint-display { (curl) }
            @if may_mint {
                p {
                    "That command does everything except the token, which it leaves "
                    "for you to mint and paste. Or let it collect its own:"
                }
                form action="/dashboard/bridges/nanoclaw/command" method="post" {
                    (components::submit("Create an install command that carries the token"))
                }
                p.muted {
                    "You get the same command with a one-time claim in it. The claim "
                    "is not the token: it is good once, it expires in minutes, and it "
                    "is spent the moment the installer trades it for a token of its "
                    "own."
                }
            } @else {
                p.muted {
                    "This command does everything except the token: when it "
                    "finishes it tells you to mint a standard consumer token and put "
                    "it in the fork's " code { ".env" } ". An admin signed in here "
                    "can instead press a button on this page and get the same command "
                    "with a one-time claim in it, which the installer trades for the "
                    "token by itself."
                }
            }
        }
    }
}

/// Human guide for the **nanoclaw** bridge — the ready-made assistant,
/// and the first consumer this catalog recommends. One command carries
/// the whole install: it places the `mwe` agent template and the
/// `add-mwe-memory` fork skill, drives nanoclaw's own setup, applies the
/// memory, connects Telegram and wires the chat.
///
/// An admin gets a second version of that command with an install claim
/// in it, and the claim is what makes the token step disappear. Without
/// one the command still runs everything else and ends by naming the
/// token as the step that is left.
///
/// No PowerShell here on purpose: nanoclaw runs on Windows only inside
/// WSL2, so the Windows path is the same `sh` command in a WSL2 shell —
/// see [`render_install_ps1`].
fn nanoclaw_guide_body(origin: &str, may_mint: bool, minted: Option<&InstallCommand>) -> Markup {
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

        (nanoclaw_command_block(&curl, may_mint, minted))

        h3 { "What it does, and the two things it asks you" }
        ol {
            li {
                "Finds your NanoClaw, or clones it at the tested ref into "
                code { "~/nanoclaw" } " (set " code { "NANOCLAW_DIR" }
                " to put it elsewhere, or to point at a fork you already have), "
                "and places the " code { "mwe" } " agent template and the "
                code { "add-mwe-memory" } " fork skill inside it. It also fetches "
                "NanoClaw's " code { "channels" } " and " code { "providers" }
                " branches: its channel adapters are copied out of them, so a "
                "warning about one of those is a channel that cannot be installed "
                "until the fetch succeeds."
            }
            li {
                strong { "Asks you two things only you know:" } " the bot token "
                "@BotFather gave you, and your own numeric Telegram id. Both can "
                "come from the environment instead ("
                code { "MWE_TELEGRAM_BOT_TOKEN" } ", "
                code { "MWE_TELEGRAM_OPERATOR_ID" } ")."
            }
            li {
                "Runs NanoClaw's own setup, which installs Node, pnpm and Docker if "
                "they are missing and builds the sandbox image here. It asks "
                strong { "two questions of its own" } " that no setting answers: "
                "“How would you like to begin?” — take " strong { "Standard setup" }
                ", the default — and “How would you like to connect to Claude?” — take "
                "the subscription sign-in, which opens your browser and keeps the "
                "token in NanoClaw's own vault. The channel, the agent, the sandbox "
                "image, the runtime and the timezone are already answered."
            }
            li {
                "Stamps the " code { "mwe" } " agent, applies the memory skill, "
                "installs Telegram and wires your chat to the agent " strong { "without "
                "the pairing code" } " (a private chat's id is your own id, so there is "
                "nothing left to learn), restarts the service and the agent containers, "
                "and then watches for your first message to confirm the turn was stored "
                "and recalled."
            }
        }
        p.muted {
            "Run it in a terminal you are sitting at: NanoClaw's two questions and the "
            "browser sign-in need one, and the installer stops with that in words rather "
            "than hanging if there is none. Re-running the whole command is how you "
            "update an install — every step of it checks what is already there. To place "
            "the files and stop, for a fork you set up yourself, run it with "
            code { "MWE_FILES_ONLY=1" } " and follow "
            code { "agents-bridges/nanoclaw/README.md" } "."
        }
        p.muted {
            strong { "Windows:" } " NanoClaw runs under WSL2, so there is no "
            "PowerShell installer. Open your WSL2 shell and run the command "
            "there."
        }

        h3 { "…or let a consumer do it" }
        p { "Paste this to any consumer that already has a shell — it reads the same "
            "instructions, and hands the command to you to run:" }
        pre.endpoint-display { (agent_line) }
        p.muted {
            "Machine-readable form: "
            a href="/bridges/nanoclaw/install.md" { "/bridges/nanoclaw/install.md" }
        }

        h2 { "Who the assistant speaks for" }
        p.muted {
            "One line per person in " code { "senderMap" } " in the fork's "
            code { "mwe.json" } " — their " code { "<channel>:<platform id>" }
            " to their user id in this memory — and a tick for each of them, plus "
            code { "guest" } ", in the consumer's delegations on the "
            (tokens_page(may_mint)) ". The installer writes the first line, yours, and "
            "the rest are added the same way as people arrive. Anybody the map does not "
            "name speaks as a guest, whose turns recall only public memory and store "
            "nothing; there is no falling back to the owner."
        }
    }
}

/// Human guide for the **Claude Code** smart-consumer bridge: register the
/// MCP server and sign in over OAuth (no token), then install the
/// strongly-recommended session-start hook. No plugins and no `curl … | sh`. The
/// consumer registers the server itself; the OAuth sign-in and the hook are
/// the operator's (it stops and asks) — see `install.md`.
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
///
/// `origin` and `claim` are the nanoclaw installer's: it talks back to
/// this server to redeem the claim and it writes the endpoint into the
/// fork. The hermes installer places files and needs neither.
fn render_install_sh(consumer: &str, origin: &str, claim: Option<&str>) -> Option<String> {
    match consumer {
        "hermes" => Some(render_install_sh_hermes()),
        "nanoclaw" => render_install_sh_nanoclaw(origin, claim),
        _ => None,
    }
}

/// The second half of the nanoclaw installer, embedded from the bridge's
/// own `install-assistant.sh` so the shell that drives nanoclaw's setup is
/// a file somebody can read, lint and run, not a wall of string pushes.
///
/// The shebang goes: this body is appended to a script that already has
/// one, and a second `#!` line halfway down a file is noise a reader has
/// to explain to themselves. `None` when the file is not embedded, which
/// makes the installer 404 rather than serve a half of it.
fn nanoclaw_install_driver() -> Option<String> {
    let file = NanoclawBridge::get("install-assistant.sh")?;
    let body = String::from_utf8_lossy(&file.data);
    Some(body.strip_prefix("#!/bin/sh\n").unwrap_or(&body).to_owned())
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
/// wizard offers this template.
///
/// Everything after that is [`nanoclaw_install_driver`], appended
/// verbatim: the two answers only a person has, the claim redemption,
/// nanoclaw's setup, the memory skill, Telegram, the restart and the
/// check. It reads `NANOCLAW_DIR`, `MWE_ORIGIN` and `MWE_CLAIM`, which is
/// why they are assigned here at the top.
///
/// The claim reaches this function already checked against
/// [`claim_is_wellformed`] — it lands inside a double-quoted shell
/// assignment in a script the operator pipes into `sh`, so the check is
/// what makes that safe, and it belongs at the edge that receives it.
///
/// `None` when the manifest carries no upstream repo or pin: an
/// installer that cloned an unpinned `main` would place a bridge beside
/// a nanoclaw it was never tested against.
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "shell ${VAR:-default} braces are not Rust format args"
)]
fn render_install_sh_nanoclaw(origin: &str, claim: Option<&str>) -> Option<String> {
    let repo = nanoclaw_upstream("repo")?;
    let pin = nanoclaw_upstream("pin")?;
    let driver = nanoclaw_install_driver()?;
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str(
        "# mwe-mcp NanoClaw bridge installer — self-contained, served by your mwe-mcp server.\n",
    );
    s.push_str(
        "# Installs the ready-made assistant: the `mwe` agent template and the\n\
         # add-mwe-memory fork skill into a NanoClaw checkout (cloned at the tested\n\
         # ref if you have none), then NanoClaw's own setup, the memory, Telegram\n\
         # and the wiring. Run it in a terminal you are sitting at.\n\
         # It needs git and curl: nanoclaw.sh installs Node, pnpm and Docker itself.\n",
    );
    s.push_str("set -eu\n\n");
    s.push_str("MWE_ORIGIN=\"");
    s.push_str(origin);
    s.push_str("\"\nMWE_CLAIM=\"");
    s.push_str(claim.unwrap_or_default());
    s.push_str("\"\n\n");
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
    // bridge and the memory — the warning says what to run before a
    // channel can be installed.
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
         \"mwe-mcp NanoClaw bridge: files placed in $NANOCLAW_DIR.\" \\\n  \
         \"  mwe agent template -> $NANOCLAW_DIR/templates/mwe/\" \\\n  \
         \"  add-mwe-memory skill -> $NANOCLAW_DIR/.claude/skills/add-mwe-memory/\"\n",
    );

    // Everything below is the bridge's own `install-assistant.sh`, appended
    // verbatim: it reads the three variables assigned at the top of this
    // script and takes the fork from "files in place" to "answering on
    // Telegram with this memory".
    s.push('\n');
    s.push_str(&driver);
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

/// Machine-readable instructions a consumer is pointed at ("Read … and
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

/// The **nanoclaw** install a consumer is pointed at. One command does
/// the whole thing, and the consumer's job is to hand it to a person: it
/// needs a terminal for nanoclaw's own two questions and for the browser
/// sign-in, and it asks for a bot token, which a consumer must never
/// collect on somebody's behalf.
fn render_install_md_nanoclaw(origin: &str) -> String {
    format!(
        "# Install the mwe-mcp NanoClaw bridge\n\
         \n\
         You are connecting **NanoClaw** — the ready-made assistant of a\n\
         first-party mwe-mcp memory server at `{origin}` — to that server. One\n\
         command, served by the same server, does the whole install:\n\
         \n\
         ```sh\n\
         curl -fsSL {origin}/bridges/nanoclaw/install.sh | sh\n\
         ```\n\
         \n\
         **Do not run it yourself. Give it to your operator.** It has to run in a\n\
         terminal a person is sitting at: NanoClaw's setup asks two questions of\n\
         its own, the Claude sign-in opens a browser, and the installer asks for a\n\
         Telegram bot token — a credential you must never collect on somebody's\n\
         behalf. Run without a terminal it stops and says so; it does not hang.\n\
         \n\
         Tell your operator what it will do and what it will ask:\n\
         \n\
         1. It finds their NanoClaw, or clones it at the tested ref into\n\
            `~/nanoclaw` (`NANOCLAW_DIR` puts it elsewhere, or points at a fork\n\
            they already have), and places the `mwe` agent template and the\n\
            `add-mwe-memory` fork skill inside it, plus NanoClaw's `channels`\n\
            and `providers` branches, which its channel adapters are copied out\n\
            of. If that path exists and is *not* a NanoClaw checkout it stops\n\
            instead of writing into it — do not work around that, ask which\n\
            fork they mean.\n\
         2. It asks them **two things only they know**: the bot token @BotFather\n\
            gave them, and their own numeric Telegram id. Both can come from the\n\
            environment instead (`MWE_TELEGRAM_BOT_TOKEN`,\n\
            `MWE_TELEGRAM_OPERATOR_ID`) if they would rather not paste at a\n\
            prompt.\n\
         3. It runs NanoClaw's own setup, which installs Node, pnpm and Docker if\n\
            they are missing and builds the sandbox image locally. NanoClaw asks\n\
            **two questions no setting answers**: “How would you like to begin?”\n\
            (take **Standard setup**, the default) and “How would you like to\n\
            connect to Claude?” (take the subscription sign-in — it opens their\n\
            browser and keeps the token in NanoClaw's own vault).\n\
         4. It stamps the `mwe` agent, applies the memory skill, installs Telegram\n\
            and wires their chat to the agent without the pairing code, restarts\n\
            the service **and** the agent containers, and watches for their first\n\
            message.\n\
         \n\
         There is **no PowerShell installer**: NanoClaw runs on Windows inside\n\
         WSL2, so on Windows the operator runs the same command in a WSL2 shell.\n\
         \n\
         ## The token, and the one thing that makes it automatic\n\
         \n\
         The command above leaves the **consumer token** to the operator: when it\n\
         finishes it tells them to issue a *standard* consumer token from the\n\
         dashboard, set it as `MWE_TOKEN` in the fork's `.env`, and restart. Until\n\
         they do, the assistant answers and remembers nothing.\n\
         \n\
         An admin signed in to the dashboard can skip that: **Bridges → NanoClaw**\n\
         has a button that mints the same command with a one-time *install claim*\n\
         in it, and the installer trades the claim for a token by itself. Tell them\n\
         that page exists; **do not attempt the token yourself**, and do not ask\n\
         them to paste one to you.\n\
         \n\
         Either way they will want the delegations: every person the assistant\n\
         speaks for, plus `guest`. Without the `guest` delegation an unrecognised\n\
         sender is refused rather than answered anonymously. A claimed install\n\
         starts with everyone enrolled plus `guest` ticked, which is a starting\n\
         point to narrow, not a decision.\n\
         \n\
         ## Afterwards\n\
         \n\
         Re-running the whole command is how an install is updated — every step\n\
         checks what is already there. `MWE_FILES_ONLY=1` places the two\n\
         directories and stops, which is the path for a fork the operator sets up\n\
         themselves; the manual steps are then in\n\
         `.claude/skills/add-mwe-memory/SKILL.md`.\n\
         \n\
         Who the assistant speaks *as* is `senderMap` in the fork's `mwe.json`:\n\
         one line per person, `<channel>:<platform id>` to their mwe user id. The\n\
         installer puts the operator in there. Anyone not listed speaks as a\n\
         guest; there is no fallback to the owner.\n\
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

/// `?claim=<code>` on the nanoclaw installer. Absent everywhere else,
/// and the only query parameter this surface reads.
#[derive(Debug, Deserialize)]
struct InstallQuery {
    #[serde(default)]
    claim: Option<String>,
}

async fn install_sh(
    Path(consumer): Path<String>,
    Host(host): Host,
    Query(q): Query<InstallQuery>,
) -> Response {
    let claim = q.claim.as_deref().map(str::trim).filter(|c| !c.is_empty());
    if claim.is_some_and(|c| !claim_is_wellformed(c)) {
        // `curl -f` never pipes a 4xx body into a shell, so refusing here
        // is the end of it — and refusing beats quoting something that is
        // not a claim into a script somebody runs.
        return (
            StatusCode::BAD_REQUEST,
            "that is not a claim this server minted\n",
        )
            .into_response();
    }
    render_install_sh(&consumer, &origin_from_host(&host), claim).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |body| {
            let mut resp = text_response(body, "text/plain; charset=utf-8");
            if claim.is_some() {
                // A claim-bearing installer is a one-time credential in a
                // URL: it must not sit in a proxy or a browser cache.
                resp.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            }
            resp
        },
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
                    /* minted */ None,
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
    render_tab_bridge_page(&state, &consumer, &host, &user, None)
}

/// The Bridges tab's per-consumer page. Shared by the GET that opens it
/// and the POST that mints an install command, so the page an admin lands
/// on after minting is the same page, with the command on it.
fn render_tab_bridge_page(
    state: &DashboardState,
    consumer: &str,
    host: &str,
    user: &SessionUser,
    minted: Option<&InstallCommand>,
) -> Response {
    let chrome = layout::Chrome::of(state);
    bridge_label(consumer).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |label| {
            Html(layout::authenticated_page(
                chrome,
                &format!("{label} bridge"),
                user,
                &guide_body(consumer, &origin_from_host(host), user.is_admin, minted),
            ))
            .into_response()
        },
    )
}

/// `POST /dashboard/bridges/nanoclaw/command` — mint an install claim and
/// show the command that carries it.
///
/// The claim is a short-lived, single-use JWT with its own
/// [`CLAIM_DEVICE_LABEL`], bound to the admin who pressed the button: it
/// is what tells [`redeem_install_claim`] whose memory user id to hand
/// back, and it is what a replay is refused against. Nothing is created
/// here — the consumer, its delegations and its token all come into being
/// when the claim is redeemed, so a command nobody runs leaves no trace.
async fn tab_mint_install_command(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Host(host): Host,
) -> Result<Response> {
    let claims = TokenClaims::new(
        admin.sender_id(),
        CLAIM_DEVICE_LABEL,
        CLAIM_RATE_LIMIT_ID,
        CLAIM_TTL,
    );
    let claim = jwt::issue(&state.secret, &claims).map_err(DashboardError::Token)?;
    let origin = origin_from_host(&host);
    tracing::info!(
        actor = admin.sender_id(),
        jti = %claims.jti,
        "dashboard minted a nanoclaw install claim"
    );
    let minted = InstallCommand {
        command: format!("curl -fsSL \"{origin}/bridges/nanoclaw/install.sh?claim={claim}\" | sh"),
        expires_utc: chrono::DateTime::from_timestamp(claims.exp, 0)
            .unwrap_or_else(chrono::Utc::now)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
    };
    Ok(render_tab_bridge_page(
        &state,
        "nanoclaw",
        &host,
        admin.session(),
        Some(&minted),
    ))
}

/// What the installer posts to redeem a claim.
#[derive(Debug, Deserialize)]
struct ClaimSubmission {
    claim: String,
}

/// `POST /bridges/nanoclaw/claim` — trade an install claim for a standard
/// consumer token.
///
/// Anonymous, because the box running the installer has no session: the
/// claim is the whole credential, which is why it is short-lived, burned
/// on first use, and checked for its own `device_label` before anything
/// else happens. The order matters — burn first, mint second — so two
/// installers racing on one command produce one token, not two.
///
/// The answer is `key=value` lines rather than JSON: its only reader is a
/// POSIX shell with no `jq`.
async fn redeem_install_claim(
    State(state): State<DashboardState>,
    HtmlForm(sub): HtmlForm<ClaimSubmission>,
) -> Response {
    // One refusal, because from the installer's side the reasons are one
    // event: this claim will not work, and the fix is a fresh command. The
    // blacklist is refreshed at redemption, so a replay usually fails at
    // `verify` rather than at the burn — telling those two apart would be
    // telling the caller which internal step noticed, not what to do.
    let refused = || {
        (
            StatusCode::FORBIDDEN,
            "this claim will not work: it is expired, already used, or not one this server \
             minted. Mint a fresh install command on the dashboard's Bridges page.\n",
        )
            .into_response()
    };

    let claim = sub.claim.trim();
    if !claim_is_wellformed(claim) {
        return refused();
    }
    let Ok(claims) = jwt::verify(&state.secret, claim, &state.pool, &state.blacklist).await else {
        return refused();
    };
    if claims.device_label != CLAIM_DEVICE_LABEL {
        return refused();
    }
    match jwt::revoke_once(
        &state.pool,
        &claims.jti,
        "nanoclaw_install_claim_redeemed",
        &claims.sender_id,
        claims.exp,
    )
    .await
    {
        Ok(true) => {},
        Ok(false) => return refused(),
        Err(e) => {
            tracing::error!(error = %e, "burning a nanoclaw install claim");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not redeem the claim\n",
            )
                .into_response();
        },
    }
    // Close the blacklist cache's window immediately, the way a
    // single-use dashboard link does, so a fast replay loses too.
    if let Err(e) = state.blacklist.refresh(&state.pool).await {
        tracing::error!(error = %e, "refreshing the blacklist after a claim redemption");
    }

    match mint_claimed_consumer_token(&state, &claims.sender_id).await {
        Ok(Ok(token)) => {
            let body = format!(
                "token={token}\nconsumer_id={NANOCLAW_CONSUMER_ID}\noperator_user_id={}\n",
                claims.sender_id
            );
            let mut resp = body.into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        },
        Ok(Err(msg)) => (StatusCode::CONFLICT, format!("{msg}\n")).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "minting the nanoclaw consumer token");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not mint the consumer token\n",
            )
                .into_response()
        },
    }
}

/// Mint the standard consumer token a redeemed claim buys, through the
/// same path the Tokens page uses — [`super::tokens::resolve_standard_sender`]
/// creates the system user and records the delegation,
/// [`super::tokens::build_claims`] shapes the token — so a consumer born
/// here is indistinguishable from one an admin issued by hand.
///
/// The delegation it starts with is **everyone enrolled, plus `guest`**.
/// That is the widest useful default and it is deliberate: an assistant
/// that cannot speak for a household member answers them as a stranger,
/// and narrowing a roster on the Tokens page is a tick, while widening one
/// after a confusing first conversation is a support question. The
/// installer says so when it finishes, and the guide says so before it
/// starts.
///
/// `Ok(Err(msg))` is a reason the operator can act on — the id is taken by
/// a person, say. The outer `Result` is a database failure.
async fn mint_claimed_consumer_token(
    state: &DashboardState,
    actor: &str,
) -> Result<std::result::Result<String, String>> {
    let users = super::tokens::fetch_user_ids(state).await?;

    // The claim was minted for an admin, but that was minutes ago and a
    // token lives a year. Two things go wrong if this is not re-read: an
    // admin who was demoted in between still buys a consumer delegated to
    // everybody, and a subject who was removed becomes the `senderMap`
    // entry the installer writes — a user id nobody has, so every turn
    // that person sends comes back `403 act_as_not_delegated`.
    let still_admin: Option<i64> =
        sqlx::query_scalar("SELECT is_admin FROM enrollment_users WHERE user_id = ?")
            .bind(actor)
            .fetch_optional(&state.pool)
            .await?;
    if still_admin != Some(1) {
        return Ok(Err(format!(
            "{actor:?} is no longer an admin of this memory, so this claim buys nothing. \
             Sign in as the admin and mint a fresh install command."
        )));
    }

    let mut allowed = users.clone();
    allowed.push(enrollment::GUEST_USER_ID.to_owned());

    let resolved = super::tokens::resolve_standard_sender(
        state,
        NANOCLAW_CONSUMER_ID,
        &allowed,
        &users,
        actor,
    )
    .await?;
    let (sender_id, is_admin) = match resolved {
        Ok(pair) => pair,
        Err(msg) => return Ok(Err(msg)),
    };

    // A nanoclaw sits on the operator's own machine or their LAN, which is
    // the case the Tokens page's default TTL profile is for, and it holds
    // no rate-limit profile of its own.
    let mut claims = super::tokens::build_claims(
        &sender_id,
        /* device_label */ NANOCLAW_CONSUMER_ID,
        /* rate_limit_id */ "default",
        /* ttl_profile */ "internal",
        is_admin,
        /* smart */ false,
    );
    claims.consumer_id = Some(NANOCLAW_CONSUMER_ID.to_owned());
    if let Err(msg) = enrollment::validate_token_identity(
        &state.pool,
        &claims.sender_id,
        claims.consumer_class,
        claims.consumer_id.is_some(),
    )
    .await
    {
        return Ok(Err(msg));
    }

    let token = jwt::issue(&state.secret, &claims).map_err(DashboardError::Token)?;
    tracing::info!(
        actor,
        consumer = NANOCLAW_CONSUMER_ID,
        jti = %claims.jti,
        delegated = allowed.len(),
        "install claim redeemed for a standard consumer token"
    );
    Ok(Ok(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// The origin every rendered-script test reads back, so a URL in an
    /// assertion is obviously the one this input produced.
    const ORIGIN: &str = "https://memory.anna.dev";

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
        // The installer's own second half is served, never copied: it
        // drives a fork from outside it.
        assert!(route_embedded_path("nanoclaw", "install-assistant.sh").is_none());
        assert!(route_embedded_path("nanoclaw", "smoke.sh").is_none());
        assert!(route_embedded_path("nanoclaw", "smoke_test.ts").is_none());
        assert!(route_embedded_path("nanoclaw", "stub_runner.py").is_none());
    }

    #[test]
    fn nanoclaw_install_sh_writes_two_trees_and_leaves_the_token_alone() {
        let sh = render_install_sh("nanoclaw", ORIGIN, None).expect("nanoclaw sh");
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
        assert!(sh.contains("$NANOCLAW_DIR/.claude/skills/add-mwe-memory/apply-headless.ts"));
    }

    /// The whole install is one script: the half rendered here places the
    /// files, and everything that turns them into a working assistant is
    /// the bridge's own `install-assistant.sh` appended to it. A served
    /// script without that half would place two directories and stop,
    /// which is exactly what this change is here to end.
    #[test]
    fn nanoclaw_install_sh_carries_the_driver_that_finishes_the_install() {
        let sh = render_install_sh("nanoclaw", ORIGIN, None).expect("nanoclaw sh");
        let driver = nanoclaw_install_driver().expect("the driver is embedded");
        assert!(
            sh.ends_with(&driver),
            "the driver must be the tail of the script"
        );
        // One shebang, at the top, where a shell looks for it.
        assert_eq!(sh.matches("#!/bin/sh").count(), 1);
        // The pieces the driver exists for, in the order it runs them.
        for needle in [
            "/bridges/nanoclaw/claim",
            "bash nanoclaw.sh </dev/tty",
            "$MWE_SKILL_DIR/apply-headless.ts",
            ".claude/skills/add-telegram",
            "messaging-groups create",
            "wirings create",
            "bash setup/lib/restart.sh",
            "restart-mwe-groups.ts",
            "[mwe] memory is on",
        ] {
            assert!(sh.contains(needle), "the installer never reaches: {needle}");
        }
        // The two questions nanoclaw asks that no setting answers are named
        // before they arrive, and the settings that answer every other one
        // are passed.
        assert!(sh.contains("How would you like to begin?"));
        assert!(sh.contains("How would you like to connect to Claude?"));
        for setting in [
            "NANOCLAW_HARDENED_IMAGE=false",
            "NANOCLAW_AGENT_PROVIDER=claude",
            "NANOCLAW_SKIP_CLAUDE_ASSIST=1",
        ] {
            assert!(sh.contains(setting), "missing {setting}");
        }
    }

    /// A claim rides the script as a shell assignment and nothing else,
    /// and the script without one still runs — it just ends by naming the
    /// token as the operator's step.
    #[test]
    fn nanoclaw_install_sh_carries_a_claim_and_works_without_one() {
        let claimed = render_install_sh("nanoclaw", ORIGIN, Some("aaa.bbb.ccc")).expect("claimed");
        assert!(claimed.contains("MWE_CLAIM=\"aaa.bbb.ccc\""));
        assert!(claimed.contains(&format!("MWE_ORIGIN=\"{ORIGIN}\"")));

        let bare = render_install_sh("nanoclaw", ORIGIN, None).expect("bare");
        assert!(bare.contains("MWE_CLAIM=\"\""));
        // Same script either way: the claim is data, not a second flow.
        assert_eq!(
            claimed.replace("aaa.bbb.ccc", ""),
            bare,
            "a claim must change one assignment and nothing else"
        );
        // Without a claim the installer says the token is still the
        // operator's, and it never invents one.
        assert!(bare.contains("the token stays yours to mint"));
        assert!(!bare.contains("MWE_TOKEN=$"));
    }

    /// The claim is interpolated into a script somebody pipes into `sh`,
    /// so what may be in one is the whole of its safety.
    #[test]
    fn only_a_jwt_shaped_claim_is_wellformed() {
        assert!(claim_is_wellformed(
            "eyJhbGciOiJIUzI1NiJ9.eyJhIjoxfQ.sig-_x"
        ));
        assert!(!claim_is_wellformed(""));
        assert!(!claim_is_wellformed("a\"; rm -rf ~ ; echo \""));
        assert!(!claim_is_wellformed("$(id)"));
        assert!(!claim_is_wellformed("`id`"));
        assert!(!claim_is_wellformed("aaa bbb"));
        assert!(!claim_is_wellformed("aaa\nbbb"));
        assert!(!claim_is_wellformed(&"a".repeat(4097)));
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
        let sh = render_install_sh("nanoclaw", ORIGIN, None).expect("nanoclaw sh");
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
        let sh = render_install_sh("nanoclaw", ORIGIN, None).expect("nanoclaw sh");
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
        // dead install: the branches are what a channel is copied out of,
        // and everything before the channel still works without them.
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
        let sh = render_install_sh("nanoclaw", ORIGIN, None).expect("nanoclaw sh");
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
        // Exactly one line of the placement half writes to that file, and
        // it is the template pick. Everything the driver writes later —
        // the bot token, the timezone, the consumer token — goes through
        // its own `env_set`, which reads and rewrites the file rather than
        // appending blind, and never puts a value on a command line.
        assert_eq!(
            sh.matches(">> \"$NANOCLAW_ENV\"").count(),
            1,
            "the placement half must append exactly one line to the fork's .env"
        );
        assert!(sh.contains("env_set TELEGRAM_BOT_TOKEN"));
        assert!(sh.contains("env_set MWE_TOKEN"));
        assert!(!sh.contains("MWE_TOKEN=$"));
        // The other setup keys reach nanoclaw through the environment of
        // the one command that runs it, never through `.env`, where
        // nothing would read them.
        assert!(!sh.contains("NANOCLAW_AGENT_PROVIDER=claude\" >>"));
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
            /* minted */ None,
        )
        .into_string();
        assert!(html.contains("WSL2"));
        assert!(!html.contains("install.ps1"));
        let md = render_install_md("nanoclaw", "https://memory.anna.dev").expect("nanoclaw md");
        assert!(md.contains("WSL2"));
        assert!(!md.contains("install.ps1"));
    }

    /// The instructions a consumer is pointed at. The install needs a
    /// person at a terminal and asks for a bot token, so the one thing
    /// this document must never do is read like something the consumer
    /// can run on its own.
    #[test]
    fn nanoclaw_install_md_hands_the_command_to_a_person() {
        let md = render_install_md("nanoclaw", ORIGIN).expect("nanoclaw md");
        assert!(md.contains(&format!(
            "curl -fsSL {ORIGIN}/bridges/nanoclaw/install.sh | sh"
        )));
        assert!(md.contains("**Do not run it yourself. Give it to your operator.**"));
        assert!(md.contains("terminal"));
        assert!(md.contains("do not attempt the token yourself"));
        assert!(md.contains("MWE_TOKEN"));
        assert!(md.contains("install claim"));
        assert!(md.contains("senderMap"));
        assert!(md.contains("guest"));
        assert!(md.contains("NANOCLAW_DIR"));
        assert!(md.contains("MWE_FILES_ONLY=1"));
    }

    #[test]
    fn nanoclaw_guide_leads_the_catalog_and_offers_the_claim() {
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
            "nanoclaw", ORIGIN, /* may_mint */ true, /* minted */ None,
        )
        .into_string();
        assert!(
            guide.contains(&format!("{ORIGIN}/bridges/nanoclaw/install.sh | sh")),
            "the guide must show the served install command"
        );
        // An admin who has not minted yet is offered the button, and no
        // claim exists until they press it.
        assert!(guide.contains("action=\"/dashboard/bridges/nanoclaw/command\""));
        assert!(!guide.contains("?claim="));
        assert!(guide.contains("href=\"/dashboard/tokens\""));
    }

    /// The minted command is the whole point of the button: it carries the
    /// claim, it says when the claim dies, and the token it will buy is
    /// named as something the installer writes rather than something the
    /// reader copies.
    #[test]
    fn a_minted_command_is_shown_once_with_its_expiry() {
        let minted = InstallCommand {
            command: format!(
                "curl -fsSL \"{ORIGIN}/bridges/nanoclaw/install.sh?claim=a.b.c\" | sh"
            ),
            expires_utc: "2026-09-07 12:34".to_owned(),
        };
        let guide = guide_body("nanoclaw", ORIGIN, true, Some(&minted)).into_string();
        assert!(guide.contains("install.sh?claim=a.b.c"));
        assert!(guide.contains("2026-09-07 12:34"));
        assert!(guide.contains("works once"));
        assert!(guide.contains("never shown on this page"));
        // The button is gone: the command on screen is the answer to it.
        assert!(!guide.contains("action=\"/dashboard/bridges/nanoclaw/command\""));
    }

    /// A reader who cannot open the Tokens page cannot mint a claim
    /// either — the button posts to an admin-only route, and offering it
    /// would be a 403 with extra steps.
    #[test]
    fn only_an_admin_is_offered_the_install_command_button() {
        let reader = guide_body("nanoclaw", ORIGIN, /* may_mint */ false, None).into_string();
        assert!(!reader.contains("action=\"/dashboard/bridges/nanoclaw/command\""));
        assert!(
            reader.contains("An admin signed in"),
            "a reader must still learn the claim exists: {reader}"
        );
    }

    #[test]
    fn install_sh_is_self_contained_and_routes_destinations() {
        let sh = render_install_sh("hermes", ORIGIN, None).expect("hermes sh");
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
        assert!(render_install_sh("nope", ORIGIN, None).is_none());
        assert!(render_install_ps1("nope").is_none());
        assert!(render_install_md("nope", "https://x").is_none());
    }

    #[test]
    fn front_body_points_a_consumer_at_the_catalog_and_a_human_at_signin() {
        let html = front_body("").into_string();
        assert!(html.contains("If you are a consumer"));
        assert!(html.contains("/bridges"));
        assert!(html.contains("href=\"/dashboard/\""));
    }

    #[test]
    fn catalog_lists_hermes_with_a_link_to_the_instructions_it_follows() {
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
            /* minted */ None,
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
            /* minted */ None,
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
        assert!(render_install_sh("claude-code", ORIGIN, None).is_none());
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
                    .header("host", "memory.anna.dev")
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

    /// The public guide, the installer and the machine-readable
    /// instructions all answer for nanoclaw. The authenticated tab is
    /// gated by the same [`bridge_label`] lookup, so a label is what
    /// decides 200 vs 404 there too.
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

    // ----------------------------------------------------------------
    // The install claim
    // ----------------------------------------------------------------

    /// A state with a real database, an enrolled admin and one other
    /// person — enough for the delegation the claim writes to have
    /// somebody in it besides the admin.
    ///
    /// The temp dir goes back to the caller: a leaked one is never
    /// removed by anything.
    async fn claim_state() -> (DashboardState, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = mwe_core::db::open_or_init(dir.path()).await.expect("db");
        for (user, admin) in [("alice", 1), ("bob", 0)] {
            sqlx::query(
                "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES (?, '[]', ?)",
            )
            .bind(user)
            .bind(admin)
            .execute(&pool)
            .await
            .expect("enrol");
        }
        let secret = mwe_core::jwt::TokenSecret::new(vec![0x5Au8; 32]).expect("secret");
        let blacklist = std::sync::Arc::new(mwe_core::jwt::BlacklistCache::new());
        let delegations = std::sync::Arc::new(mwe_core::delegations::DelegationCache::new());
        (
            DashboardState::new(pool, secret, blacklist, delegations),
            dir,
        )
    }

    fn mint_claim(state: &DashboardState, subject: &str) -> String {
        let claims = TokenClaims::new(subject, CLAIM_DEVICE_LABEL, CLAIM_RATE_LIMIT_ID, CLAIM_TTL);
        jwt::issue(&state.secret, &claims).expect("issue")
    }

    async fn post_claim(state: &DashboardState, claim: &str) -> Response {
        claim_router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/bridges/nanoclaw/claim")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("claim={claim}")))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// The whole point of a claim: the installer hands one over and gets
    /// back a standard consumer token, its own memory user id, and a
    /// delegation roster it did not have to fill in.
    #[tokio::test]
    async fn a_claim_buys_a_standard_consumer_token_delegated_to_everybody() {
        let (state, _workdir) = claim_state().await;
        let claim = mint_claim(&state, "alice");

        let resp = post_claim(&state, &claim).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "a response carrying a credential must not be cached"
        );
        let body = body_string(resp).await;
        assert!(body.contains(&format!("consumer_id={NANOCLAW_CONSUMER_ID}\n")));
        assert!(
            body.contains("operator_user_id=alice\n"),
            "the installer needs the admin's memory user id for senderMap: {body}"
        );
        let token = body
            .lines()
            .find_map(|l| l.strip_prefix("token="))
            .expect("a token line");

        // It is a real consumer token, standard, and it names the consumer
        // whose delegations were just written.
        let claims = jwt::verify(&state.secret, token, &state.pool, &state.blacklist)
            .await
            .expect("the minted token verifies");
        assert_eq!(claims.sender_id, NANOCLAW_CONSUMER_ID);
        assert!(!claims.consumer_class.is_smart());
        assert_eq!(claims.consumer_id.as_deref(), Some(NANOCLAW_CONSUMER_ID));
        assert!(!claims.is_admin, "a consumer identity is never an admin");

        // Everybody enrolled, plus guest — the roster the guide and the
        // installer both say to narrow afterwards.
        let allowed: String = sqlx::query_scalar(
            "SELECT allowed_sender_ids FROM consumer_delegations WHERE consumer_id = ?",
        )
        .bind(NANOCLAW_CONSUMER_ID)
        .fetch_one(&state.pool)
        .await
        .expect("a delegation row");
        let allowed: Vec<String> = serde_json::from_str(&allowed).expect("json");
        assert!(allowed.contains(&"alice".to_owned()));
        assert!(allowed.contains(&"bob".to_owned()));
        assert!(
            allowed.contains(&"guest".to_owned()),
            "without guest an unrecognised sender is refused, not answered anonymously"
        );

        // And the consumer exists as a credential-less system user.
        let is_system = mwe_core::enrollment::is_system_user(&state.pool, NANOCLAW_CONSUMER_ID)
            .await
            .expect("system user");
        assert!(is_system);
    }

    /// A claim outlives the click that made it, and a token outlives the
    /// claim by a year. If the person it was minted for is no longer an
    /// admin, it buys nothing — and it certainly does not name them in a
    /// `senderMap` that would then refuse their every turn.
    #[tokio::test]
    async fn a_claim_from_somebody_who_is_no_longer_an_admin_buys_nothing() {
        let (state, _workdir) = claim_state().await;
        let claim = mint_claim(&state, "alice");
        sqlx::query("UPDATE enrollment_users SET is_admin = 0 WHERE user_id = 'alice'")
            .execute(&state.pool)
            .await
            .expect("demote");

        let resp = post_claim(&state, &claim).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert!(body_string(resp).await.contains("no longer an admin"));
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM consumer_delegations")
            .fetch_one(&state.pool)
            .await
            .expect("count");
        assert_eq!(
            rows, 0,
            "no consumer is created for a claim that buys nothing"
        );
    }

    /// A claim is a credential in a URL: it travels through a shell
    /// history and a server log, so it survives exactly one redemption.
    #[tokio::test]
    async fn a_claim_works_once_and_the_replay_is_refused() {
        let (state, _workdir) = claim_state().await;
        let claim = mint_claim(&state, "alice");

        assert_eq!(post_claim(&state, &claim).await.status(), StatusCode::OK);

        let replay = post_claim(&state, &claim).await;
        assert_eq!(replay.status(), StatusCode::FORBIDDEN);
        let msg = body_string(replay).await;
        assert!(
            msg.contains("already used") && msg.contains("Mint a fresh install command"),
            "the refusal must name the fix, not the internal step: {msg}"
        );
    }

    /// The same secret signs session cookies, MCP bearers and claims, so
    /// the label is what keeps them apart. A stolen session cookie must
    /// not buy a consumer token that outlives it by a year.
    #[tokio::test]
    async fn a_session_token_is_not_an_install_claim() {
        let (state, _workdir) = claim_state().await;
        let session = TokenClaims::new(
            "alice",
            crate::auth::session::SESSION_DEVICE_LABEL,
            CLAIM_RATE_LIMIT_ID,
            CLAIM_TTL,
        );
        let token = jwt::issue(&state.secret, &session).expect("issue");

        let resp = post_claim(&state, &token).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // And nothing was created on the way to refusing.
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM consumer_delegations")
            .fetch_one(&state.pool)
            .await
            .expect("count");
        assert_eq!(rows, 0, "a refused claim must leave no consumer behind");
    }

    /// A claim that is not shaped like one is refused at the door, by the
    /// same check that decides whether it is safe to write into a served
    /// shell script.
    #[tokio::test]
    async fn a_claim_that_is_not_one_is_refused_before_anything_happens() {
        let (state, _workdir) = claim_state().await;
        let resp = post_claim(&state, "not+a+claim").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM token_blacklist")
            .fetch_one(&state.pool)
            .await
            .expect("count");
        assert_eq!(rows, 0, "nothing is burned for a claim that is not one");
    }

    /// The installer is a script somebody pipes into `sh`. A claim that
    /// could change what that script does is refused, and `curl -f` never
    /// pipes a 4xx body anywhere.
    #[tokio::test]
    async fn the_installer_is_not_served_with_a_claim_it_could_not_have_minted() {
        let resp = public_site_router()
            .oneshot(
                Request::builder()
                    .uri("/bridges/nanoclaw/install.sh?claim=a%22%3B%20id%20%3B%22")
                    .header("host", "memory.anna.dev")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A claim-bearing installer is a one-time credential in a URL, so it
    /// must not sit in a cache; the same script without one is the public
    /// artefact it always was.
    #[tokio::test]
    async fn only_a_claim_bearing_installer_refuses_to_be_cached() {
        let fetch = |uri: &'static str| async move {
            public_site_router()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("host", "memory.anna.dev")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        };

        let claimed = fetch("/bridges/nanoclaw/install.sh?claim=aaa.bbb.ccc").await;
        assert_eq!(claimed.status(), StatusCode::OK);
        assert_eq!(
            claimed.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert!(
            body_string(claimed)
                .await
                .contains("MWE_CLAIM=\"aaa.bbb.ccc\"")
        );

        let bare = fetch("/bridges/nanoclaw/install.sh").await;
        assert_eq!(bare.status(), StatusCode::OK);
        assert_eq!(
            bare.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=300, must-revalidate"
        );
    }

    /// The Tokens page is admin-only, so the guide links to it for
    /// whoever may open it and names it in plain words for everybody
    /// else — a reader following that link would meet a 403.
    #[test]
    fn the_tokens_page_is_a_link_only_where_it_opens() {
        let for_admin = guide_body("nanoclaw", "https://memory.anna.dev", true, None).into_string();
        assert!(
            for_admin.contains("href=\"/dashboard/tokens\""),
            "{for_admin}"
        );

        let for_reader =
            guide_body("nanoclaw", "https://memory.anna.dev", false, None).into_string();
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
