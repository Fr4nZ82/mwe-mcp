// SPDX-License-Identifier: AGPL-3.0-or-later
//! `mwe-mcp` — MCP server binary.
//!
//! The CLI wires the storage floor and identity layer together so
//! the binary can bootstrap a workdir, manage JWTs, and bring
//! up a guarded process, and `serve` brings up the MCP transport (rmcp)
//! and the tool surface.

#![forbid(unsafe_code)]

use mwe_mcp_server::backup_scheduler;
use mwe_mcp_server::env_loader::{self, WriteOutcome};
use mwe_mcp_server::mcp;
use mwe_mcp_server::rem_scheduler;

use std::io::{IsTerminal, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use clap::{Parser, Subcommand, ValueEnum};
use mwe_core::{
    config::Config,
    db,
    delegations::DelegationCache,
    diagnostics,
    embedder::Embedder,
    jwt::{
        self, BlacklistCache, ConsumerClass, DEFAULT_EXPOSED_TTL, DEFAULT_INTERNAL_TTL,
        MIN_SECRET_BYTES, TokenClaims, TokenSecret,
    },
    lockfile, prompts,
    reindex::{self, SAFETY_NET_INTERVAL},
    wal::{self, DEFAULT_STALE_AFTER, NoopInverse},
    watcher::{WikiWatcher, sweep_stale_markers},
    wiki::WikiTree,
    workdir_security::{self, UserClass},
};
use mwe_dashboard::{DashboardState, MemoryHandles};
use mwe_mcp_server::tracing_setup;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tracing::{info, warn};

use mwe_mcp_server::mcp::state::McpState;

/// Env var that holds the HS256 secret used to sign every JWT this
/// deployment issues. Required for [`Command::Serve`] and the
/// `token-*` family.
const SECRET_ENV: &str = "MWE_TOKEN_SECRET";

/// CLI entry point per mwe-mcp.
#[derive(Debug, Parser)]
#[command(
    name = "mwe-mcp",
    version,
    about = "Memory Wiki Engine — agent-agnostic MCP server",
    long_about = None,
)]
struct Cli {
    /// Workdir holding `engine.db`, `.mwe-mcp.lock`, and the markdown
    /// SSOT under `wikis/`. Required by every sub-command that touches
    /// durable state.
    #[arg(long, global = true, default_value = "./work")]
    workdir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Bootstrap workdir: create directories, apply migrations, generate
    /// a fresh `MWE_TOKEN_SECRET` if absent. Identity (users, groups,
    /// admin) is created later through the dashboard first-run wizard
    /// per the setup wizard and identity model.
    ///
    /// **Optional**: `serve` self-bootstraps the same workdir state on
    /// first boot (directories, migrations, secret). `init` exists for
    /// deterministic headless / `IaC` provisioning where a pre-boot step
    /// with an explicit `--llm-profile` is wanted; the interactive path
    /// is just `serve` + the dashboard wizard.
    Init {
        /// Seed `mwe-mcp.config.yaml` with the chosen LLM profile.
        /// Skipped when the file already exists. Accepted values:
        /// `all-local` (default), `hybrid`, `all-api`, `custom`. See
        /// the [config schema](../../../docs/protocol/config-schema.md) for the
        /// profile presets.
        #[arg(long, default_value = "all-local")]
        llm_profile: String,

        /// Overwrite `mwe-mcp.config.yaml` even if it already exists.
        /// Off by default to keep `init` idempotent on populated
        /// workdirs.
        #[arg(long, default_value_t = false)]
        force_config: bool,
    },

    /// Start the MCP server (HTTP daemon).
    ///
    /// Brings up Axum on one listener for `/mcp` (Streamable HTTP per
    /// `rmcp`, JWT-gated) and `/dashboard/*` (built-in web UI). mwe-mcp
    /// is HTTP-only by design: every consumer — local or remote —
    /// connects over HTTP with a per-call JWT. There is no stdio
    /// transport (the dashboard, where proposals are approved, is
    /// mandatory, and the single-writer lockfile precludes a second
    /// stdio process on the same workdir).
    ///
    /// `serve` is **self-bootstrapping**: on an empty workdir it creates
    /// the directories, applies migrations, and generates + persists a
    /// fresh `MWE_TOKEN_SECRET` (0600) if absent, then boots with an
    /// empty LLM config. A prior `mwe-mcp init` is **not** required —
    /// the admin completes identity and LLM config from the dashboard
    /// first-run wizard at `/dashboard/setup`.
    Serve {
        /// Bind address. Omit it on an interactive terminal and `serve` asks
        /// whether to expose the server to other machines; otherwise it
        /// defaults to `127.0.0.1` (this machine only). Pass `0.0.0.0` to
        /// expose it on the LAN (port-forwardable / behind a reverse proxy
        /// or a tunnel) — the per-call JWT stays the only wire gate, so put
        /// TLS in front for anything past a trusted LAN.
        #[arg(long)]
        bind: Option<IpAddr>,

        /// TCP port to listen on. Omitted: prompted on an interactive bare
        /// `serve`, otherwise `8742`.
        #[arg(long)]
        port: Option<u16>,

        /// Skip the dedicated-user startup gate. By default the daemon
        /// refuses to boot as root or as a login-capable account, because
        /// the on-disk workdir is cleartext and any process that same user
        /// runs (a co-located agent's file tool) could read every
        /// memory-wiki fragment, bypassing the per-reader ACL. Pass this
        /// only where a dedicated service account is genuinely impossible
        /// (some managed/remote hosts, containers) — the co-location
        /// boundary is then NOT enforced (roadmap group 14).
        #[arg(long = "bypassdedicateduser")]
        bypassdedicateduser: bool,
    },

    /// Issue a JWT for the given sender + device.
    TokenIssue {
        /// `sender_id` claim — must match a `users[].id` in enrollment.
        #[arg(long)]
        sender: String,
        /// `device_label` claim, free-form (e.g. "claude-code-pclavoro").
        #[arg(long)]
        device: String,
        /// `rate_limit_id` claim, referenced from `mwe-mcp.config.yaml`.
        #[arg(long, default_value = "default")]
        rate_limit_id: String,
        /// TTL profile. `internal` = 1y, `exposed` = 30d.
        /// The same module path is reused by `dashboard_link` with
        /// `--ttl=session` to mint 10-minute tokens.
        #[arg(long, default_value = "internal", value_parser = ["internal", "exposed"])]
        ttl: String,
        /// Mark the token as `isAdmin` (UI gating only, never bypasses
        /// ACL).
        #[arg(long)]
        is_admin: bool,
        /// Optional `consumer_id` claim for multi-consumer ack
        /// tracking.
        #[arg(long)]
        consumer_id: Option<String>,
        /// Consumer class to bake into the token. `smart`
        /// authorises the `wiki_admin_*` tool family; `standard` is the
        /// default and is wire-omitted so older tooling keeps working
        /// unchanged.
        #[arg(long, value_enum, default_value_t = ConsumerClassArg::Standard)]
        class: ConsumerClassArg,
    },

    /// Revoke a JWT by jti. Inserts a row in `token_blacklist`.
    TokenRevoke {
        /// JWT id to blacklist.
        jti: String,
        /// Human-readable reason. Required for audit.
        #[arg(long)]
        reason: String,
        /// Actor performing the revoke. Defaults to `$USER` or `cli`.
        #[arg(long)]
        revoked_by: Option<String>,
        /// Original `exp` of the token (unix seconds). Recorded as
        /// `expires_at` so GC can drop the row once it could no longer
        /// authenticate anyway. Defaults to "now + 1y".
        #[arg(long)]
        original_exp: Option<i64>,
    },

    /// List revoked tokens. Only lists what we persist —
    /// "active tokens" (issued but not revoked, not expired) is not
    /// computable today because we do not log issuance server-side
    /// (token = self-contained identity).
    TokenList,

    /// Break-glass password recovery: mint a fresh `user_invitations`
    /// row for `user` and print the dashboard accept URL. The admin
    /// shares it with the user out of band; the user lands on
    /// `/dashboard/accept-invite/<id>`, picks a new password, and
    /// the existing `user_credentials` row (if any) is overwritten
    /// when they submit. The admin never sees the password
    /// (see the setup wizard and identity model).
    AdminReset {
        /// User id whose credential is being reset. Must exist in
        /// `enrollment_users`.
        #[arg(long)]
        user: String,
        /// Invitation lifetime, in hours. Default matches the
        /// `user_invitations.expires_at` convention.
        #[arg(long, default_value_t = 24)]
        ttl_hours: u32,
        /// Actor recording the reset. Defaults to `$USER` or `cli`.
        #[arg(long)]
        invited_by: Option<String>,
        /// Also clear the user's two-factor (TOTP) enrollment — the
        /// break-glass for a lost authenticator (roadmap 28). The user
        /// re-enrolls after they sign in with the new password.
        #[arg(long, default_value_t = false)]
        clear_2fa: bool,
    },

    /// Health check: lockfile, DB integrity, WAL recovery scan,
    /// migration count, token secret presence + length.
    Doctor,

    /// Major upgrade entrypoint. Today's behaviour is the **floor**:
    /// re-runs compile-time embedded migrations, re-seeds the
    /// bundled operational prompts from `include_str!`, and reports what
    /// changed.
    ///
    /// Future major upgrades (embedding model change, schema breaking,
    /// bundled type version bump) plug their per-version handler into
    /// `cmd_migrate` and dispatch by `--from <version> --to <version>`
    /// or by detecting drift against `engine.db.app_version`. For now
    /// the command is dispatch-by-default and the per-version code
    /// path lands when the first such upgrade is needed.
    Migrate {
        /// Only print what would change; do not touch the DB or the
        /// filesystem.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },

    /// Hot workdir snapshot for backup / disaster recovery: a
    /// point-in-time copy of `engine.db` (`VACUUM INTO`, taken first)
    /// followed by the markdown tree + config. Safe next to a live
    /// `mwe-mcp serve` — no lockfile is taken and the source DB is
    /// opened read-only. Restore is a documented manual procedure (see
    /// the backup-and-dr design note): stop the server, replace the
    /// workdir with the snapshot, start.
    Backup {
        /// Destination directory for the snapshot. Created if missing;
        /// must be empty and outside the workdir.
        #[arg(long)]
        out: PathBuf,
    },

    /// REM cycle operations — the same orchestrator the long-lived
    /// server runs nightly, exposed as an escape hatch for cron-driven
    /// deployments and one-shot manual triggers.
    Rem {
        #[command(subcommand)]
        command: RemCommand,
    },

    /// Recall operations (measurement tooling).
    Recall {
        #[command(subcommand)]
        command: RecallCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RecallCommand {
    /// Replay a YAML gold set against the workdir and score the flat-RAG
    /// baseline (hit@1 / hit@3 / coverage) against recall-as-navigation
    /// (coverage + deviating catches). Read-only: no lockfile (it may
    /// run next to a live `mwe-mcp serve`) and no recall-counter bumps
    /// (synthetic queries must not pollute the recency signal).
    Eval {
        /// Path to the gold-query YAML file (`queries:` list — see the
        /// recall-pipeline design note for the schema).
        #[arg(long)]
        gold: PathBuf,
        /// Skip the navigator even when the `navigator` LLM slot is
        /// configured — flat baseline only.
        #[arg(long, default_value_t = false)]
        flat_only: bool,
    },
}

#[derive(Debug, Subcommand)]
#[allow(
    clippy::enum_variant_names,
    reason = "the `Run` prefix mirrors the `rem run-*` CLI subcommand family"
)]
enum RemCommand {
    /// Run one full REM cycle synchronously and print a one-line
    /// summary. Acquires the workdir lockfile so it never races with a
    /// running `mwe-mcp serve` instance — call it from cron when the
    /// long-lived server is off, or from a manual session when the
    /// operator wants to trigger maintenance ahead of the next tick.
    RunCycle,
    /// Run one light-dream cycle synchronously (promote buffered
    /// captures into `fact_index`) and print a one-line summary. The
    /// frequent, cheap counterpart to `run-cycle`; acquires the workdir
    /// lockfile so it never races with a running `mwe-mcp serve`.
    RunLight,
    /// Run one narrative compile pass synchronously: (incrementally)
    /// rebuild the compilation plan and compile the dirty pages into prose.
    /// Needs the `cronista` (+ `hub_writer`) LLM slots configured; acquires
    /// the workdir lockfile.
    RunCompile,
}

/// CLI surface for the `consumer_class` JWT claim. Mirrors
/// [`mwe_core::jwt::ConsumerClass`] so `clap` can parse `--class smart`
/// directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ConsumerClassArg {
    /// Conversational consumer (openclaw, hermes, nanoclaw). Default.
    Standard,
    /// Consumer with its own LLM subscription (Claude Code, Cowork, …).
    /// Required to call the `wiki_admin_*` tool family.
    Smart,
}

impl From<ConsumerClassArg> for ConsumerClass {
    fn from(value: ConsumerClassArg) -> Self {
        match value {
            ConsumerClassArg::Standard => Self::Standard,
            ConsumerClassArg::Smart => Self::Smart,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI first so we know the workdir for config lookup.
    let cli = Cli::parse();

    // Apply `<workdir>/mwe-mcp.env` to the process env *before* anything
    // else reads `std::env`: that way `RUST_LOG`, `MWE_TOKEN_SECRET`,
    // `ANTHROPIC_API_KEY`, and the `MWE_LLM_<slot>_*` overrides all see
    // the operator's intent. A missing file is a no-op; a malformed file
    // is a fatal error surfaced here so the operator catches it before
    // any subsystem is touched.
    if let Err(e) = env_loader::load_workdir_env(&cli.workdir) {
        eprintln!("mwe-mcp: failed to load workdir env file: {e:#}");
        std::process::exit(2);
    }

    // Load mwe-mcp.config.yaml from the workdir to pick up `logging.level`.
    // Absent file ⇒ defaults (info). A malformed file is a fatal error
    // surfaced to the operator's terminal — we do NOT silently fall back,
    // otherwise an operator's typo in the config gets swallowed and the
    // wrong log level wins.
    let config = match Config::load(&cli.workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mwe-mcp: failed to load config: {e}");
            std::process::exit(2);
        },
    };

    // Tracing precedence (a rotating file sink extends it — see
    // ../../../the logging design note for the extension rationale):
    //   1. RUST_LOG env var if set — operator override always wins.
    //   2. logging.level from mwe-mcp.config.yaml.
    //   3. info (default).
    //
    // Sinks (both follow the precedence above):
    //   - `stderr` is always installed (keeps stdout clean; an admin
    //     attaching interactively wants live output).
    //   - `<workdir>/logs/mwe-mcp.log` (or the operator-configured path)
    //     when `logging.file_rotation` is not `disabled`. Default is
    //     `daily`. The `_log_guard` value must be held for the
    //     remainder of the process: dropping it flushes the
    //     non-blocking appender's pending writes.
    let _log_guard = tracing_setup::install(&cli.workdir, &config);

    info!(
        log_level = ?config.logging.level,
        file_rotation = config.logging.file_rotation.yaml_name(),
        file_path = ?config.logging.resolved_file_path(&cli.workdir),
        workdir = %cli.workdir.display(),
        "mwe-mcp: starting"
    );

    // Install the process-wide Claude Code login store (captures the
    // workdir). Any slot wired with `api_key_env: claude-code` resolves
    // its bearer token through this store; the dashboard login routes
    // write into it. Cheap and idempotent — installed for every
    // subcommand so `serve`, `doctor`, and the `rem`/`recall` CLIs all
    // share one store. Test/personal use only (see mwe_core::oauth).
    mwe_core::oauth::install_global_store(Arc::new(mwe_core::oauth::default_store(&cli.workdir)));

    // Install the process-wide training spool (captures the workdir).
    // Every LLM backend built through `build_backend` records its
    // prompt/completion pairs into it when enabled — the flag starts
    // from `training_spool.enabled` in the YAML and is hot-toggled by
    // the dashboard panel. Installed for every subcommand so `serve`
    // and the `rem` CLI runs spool alike.
    mwe_core::training_spool::install_global(Arc::new(
        mwe_core::training_spool::TrainingSpool::new(&cli.workdir, config.training_spool.enabled),
    ));

    match cli.command {
        Command::Init {
            llm_profile,
            force_config,
        } => cmd_init(&cli.workdir, &llm_profile, force_config).await,
        Command::Serve {
            bind,
            port,
            bypassdedicateduser,
        } => cmd_serve_http(&cli.workdir, &config, bind, port, bypassdedicateduser).await,
        Command::TokenIssue {
            sender,
            device,
            rate_limit_id,
            ttl,
            is_admin,
            consumer_id,
            class,
        } => {
            cmd_token_issue(
                &cli.workdir,
                &sender,
                &device,
                &rate_limit_id,
                &ttl,
                is_admin,
                consumer_id,
                class.into(),
            )
            .await
        },
        Command::TokenRevoke {
            jti,
            reason,
            revoked_by,
            original_exp,
        } => cmd_token_revoke(&cli.workdir, &jti, &reason, revoked_by, original_exp).await,
        Command::TokenList => cmd_token_list(&cli.workdir).await,
        Command::AdminReset {
            user,
            ttl_hours,
            invited_by,
            clear_2fa,
        } => cmd_admin_reset(&cli.workdir, &user, ttl_hours, invited_by, clear_2fa).await,
        Command::Doctor => cmd_doctor(&cli.workdir).await,
        Command::Migrate { dry_run } => cmd_migrate(&cli.workdir, dry_run).await,
        Command::Backup { out } => cmd_backup(&cli.workdir, &out).await,
        Command::Rem { command } => match command {
            RemCommand::RunCycle => cmd_rem_run_cycle(&cli.workdir, &config).await,
            RemCommand::RunLight => cmd_rem_run_light(&cli.workdir, &config).await,
            RemCommand::RunCompile => cmd_rem_run_compile(&cli.workdir, &config).await,
        },
        Command::Recall { command } => match command {
            RecallCommand::Eval { gold, flat_only } => {
                cmd_recall_eval(&cli.workdir, &config, &gold, flat_only).await
            },
        },
    }
}

/// Bootstrap a workdir.
///
/// Steps:
/// 1. Acquire the lockfile (fails fast with `409 instance_running` if
///    held — prevents two `init` calls from racing on the same dir).
/// 2. Open `engine.db`, which applies every migration.
/// 3. Seed `mwe-mcp.config.yaml` from the chosen LLM profile (preserved
///    on re-run unless `--force-config` is passed).
/// 4. Write `<workdir>/mwe-mcp.env` with the generated
///    `MWE_TOKEN_SECRET`. The file is `chmod 0o600` on unix and
///    preserved on re-run unless `--force-config` is passed; the
///    workdir env loader picks it up on every subsequent invocation,
///    so the operator never has to `source` it manually.
///
/// Identity (users, groups, the first admin) is **not** seeded here:
/// the dashboard owns the identity lifecycle (see [identity-and-acl.md](../../../docs/concepts/identity-and-acl.md)),
/// and the first-run setup wizard at `/dashboard/setup` creates the
/// first admin on the next `serve`.
async fn cmd_init(workdir: &Path, llm_profile: &str, force_config: bool) -> Result<()> {
    info!(workdir = %workdir.display(), llm_profile, "mwe-mcp init: starting");

    let profile = mwe_core::config::LlmProfile::parse(llm_profile).map_err(|bad| {
        anyhow!("unknown --llm-profile {bad:?} (accepted: all-local, hybrid, all-api, custom)")
    })?;

    let _lock = lockfile::acquire(workdir).map_err(|e| anyhow!("lockfile: {e}"))?;
    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;

    let n_tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' \
         AND name NOT LIKE '_sqlx_%'",
    )
    .fetch_one(&pool)
    .await?;

    // Bootstrap the memory-wiki tree (creates `wikis/` if absent).
    // Idempotent: re-running `init` on a populated workdir leaves
    // operator edits untouched.
    WikiTree::open(workdir).context("opening wikis/ tree")?;

    let config_path = workdir.join("mwe-mcp.config.yaml");
    let config_status = seed_config_file(&config_path, profile, force_config)?;

    // Materialise the bundled operational prompts into
    // `<workdir>/prompts/<name>.md`. The seeder is idempotent: an
    // operator who has hand-edited a prompt does not lose changes
    // when `init` reruns; new prompts shipped in a binary upgrade
    // land cleanly next to the existing ones.
    let core_prompts = prompts::seed_bundled_into(workdir, prompts::BUNDLED)
        .context("seeding bundled mwe-core prompts")?;
    let dash_prompts = prompts::seed_bundled_into(workdir, mwe_dashboard::BUNDLED_PROMPTS)
        .context("seeding bundled mwe-dashboard prompts")?;
    let prompts_seed = core_prompts.merged(&dash_prompts);

    // Always generate a fresh secret candidate; the helper below
    // decides whether to actually persist it. We mint here (rather
    // than inside the helper) so the env-file lookup path stays a
    // pure I/O concern.
    let candidate = TokenSecret::generate();
    let env_outcome =
        env_loader::write_env_file_if_needed(workdir, &candidate.export_hex(), force_config)
            .context("writing mwe-mcp.env")?;
    let env_status = match &env_outcome {
        WriteOutcome::Wrote { path, chmod_0600 } => {
            if *chmod_0600 {
                format!("wrote to {} (chmod 600)", path.display())
            } else {
                format!(
                    "wrote to {} (chmod skipped — non-unix host)",
                    path.display()
                )
            }
        },
        WriteOutcome::Preserved { path } => {
            format!(
                "preserved {} (use --force-config to overwrite)",
                path.display()
            )
        },
    };

    println!("workdir         : {}", workdir.display());
    println!("engine.db tables: {n_tables}");
    println!("identity        : empty (run the dashboard setup wizard on first serve)");
    println!(
        "prompts         : seeded={} preserved={}",
        prompts_seed.created, prompts_seed.preserved
    );
    println!(
        "llm config      : profile={} {}",
        profile.yaml_name(),
        config_status
    );
    println!("secret          : {env_status}");
    if matches!(env_outcome, WriteOutcome::Preserved { .. }) {
        // Preserve mode never updates the existing secret; surface it
        // so the operator does not assume a fresh value landed.
        info!("{SECRET_ENV}: existing mwe-mcp.env preserved — secret not rotated");
    }

    Ok(())
}

/// Seed `mwe-mcp.config.yaml` with the chosen profile's defaults.
///
/// Returns a status string for the operator-facing summary:
/// `wrote <path>` | `preserved (already present — use --force-config to overwrite)`.
fn seed_config_file(
    path: &Path,
    profile: mwe_core::config::LlmProfile,
    force: bool,
) -> Result<String> {
    if path.exists() && !force {
        return Ok(format!(
            "preserved {} (use --force-config to overwrite)",
            path.display()
        ));
    }
    let llm = profile.build();
    let config = mwe_core::config::Config {
        llm,
        ..Default::default()
    };
    let yaml = serde_yaml::to_string(&config).context("serialising mwe-mcp.config.yaml")?;
    std::fs::write(path, yaml).with_context(|| format!("writing {}", path.display()))?;
    Ok(format!("wrote {}", path.display()))
}

/// Major upgrade entrypoint. See [`Command::Migrate`] for the
/// scope statement.
///
/// Today's behaviour is the floor:
///
/// 1. Re-run the compile-time embedded migrations (`db::open_or_init`
///    is idempotent; already-applied migrations are no-ops).
/// 2. Re-seed the bundled operational prompts from `include_str!`.
///
/// `--dry-run` prints what would change without touching the DB or
/// filesystem.
async fn cmd_migrate(workdir: &Path, dry_run: bool) -> Result<()> {
    info!(workdir = %workdir.display(), dry_run, "mwe-mcp migrate: starting");
    let _lock = lockfile::acquire(workdir).map_err(|e| anyhow!("lockfile: {e}"))?;
    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;
    // Open the tree for its `wikis/`-creation side effect.
    WikiTree::open(workdir).context("opening wikis/ tree")?;

    let n_tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' \
         AND name NOT LIKE '_sqlx_%'",
    )
    .fetch_one(&pool)
    .await?;

    // Also re-seed the bundled operational prompts on a
    // major upgrade. Idempotent: an operator who has hand-edited a
    // `<workdir>/prompts/<name>.md` keeps the edit; prompts added in
    // the new binary version flow in next to the existing ones.
    // Drift detection (the bundled body has changed but the workdir
    // file is unchanged) is not handled here — the operator
    // sees the new file only when there was no override yet.
    let prompts_seed_counts: (usize, usize) = if dry_run {
        let total = prompts::BUNDLED.len() + mwe_dashboard::BUNDLED_PROMPTS.len();
        println!("(dry-run) would re-seed {total} bundled prompts");
        (0, 0)
    } else {
        let core_prompts = prompts::seed_bundled_into(workdir, prompts::BUNDLED)
            .context("seeding bundled mwe-core prompts")?;
        let dash_prompts = prompts::seed_bundled_into(workdir, mwe_dashboard::BUNDLED_PROMPTS)
            .context("seeding bundled mwe-dashboard prompts")?;
        let report = core_prompts.merged(&dash_prompts);
        (report.created, report.preserved)
    };

    println!("workdir          : {}", workdir.display());
    println!("engine.db tables : {n_tables}");
    println!(
        "prompts          : seeded={} preserved={}",
        prompts_seed_counts.0, prompts_seed_counts.1
    );
    if dry_run {
        println!("(dry-run) no changes written");
    } else {
        println!("migrate          : ok (re-applied migrations + re-seeded bundled prompts)");
    }
    Ok(())
}

/// Hot workdir snapshot (see [`Command::Backup`]). Deliberately no
/// lockfile: the snapshot is read-only towards the workdir and is
/// designed to run next to a live server.
async fn cmd_backup(workdir: &Path, out: &Path) -> Result<()> {
    info!(workdir = %workdir.display(), out = %out.display(), "mwe-mcp backup: starting");
    let report = mwe_core::backup::snapshot_workdir(workdir, out)
        .await
        .context("workdir snapshot")?;
    println!("workdir       : {}", workdir.display());
    println!("snapshot      : {}", out.display());
    println!(
        "engine.db     : {} bytes (VACUUM INTO, point-in-time)",
        report.db_bytes
    );
    println!(
        "files         : {} copied ({} bytes)",
        report.files_copied, report.bytes_copied
    );
    println!("restore       : stop the server, replace the workdir with the snapshot, start");
    Ok(())
}

/// Run one REM cycle synchronously and print a one-line summary. The
/// CLI escape hatch for cron-driven deployments and manual triggers
/// (the headless side of the REM-cycle scheduling story).
///
/// Acquires the workdir lockfile so a concurrent `mwe-mcp serve`
/// surface — whose own scheduler may be about to fire — cannot race
/// against this CLI invocation. Set `rem.schedule.mode: disabled` in
/// `mwe-mcp.config.yaml` when you intend to drive REM from an external
/// scheduler so the in-process scheduler stays quiet.
async fn cmd_rem_run_cycle(workdir: &Path, config: &Config) -> Result<()> {
    info!(workdir = %workdir.display(), "mwe-mcp rem run-cycle: starting");

    let lock = lockfile::acquire(workdir)
        .map_err(|e| anyhow!("lockfile: {e} (is `mwe-mcp serve` already running?)"))?;
    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;
    let tree = WikiTree::open(workdir).context("opening wikis/ tree")?;
    let embedder: Arc<dyn Embedder> = config
        .embedding
        .build_embedder()
        .await
        .context("building embedder")?;
    let llms = rem_scheduler::build_backends(&config.llm)?.ok_or_else(|| {
        anyhow!(
            "rem run-cycle: `llm.hub_writer` or `llm.rem_dedup_semantic` are not configured \
             in mwe-mcp.config.yaml; both are required to run a cycle"
        )
    })?;
    let policy = config.rem.resolved_policy();
    let report = rem_scheduler::run_once(&pool, &tree, embedder, &llms, &policy)
        .await
        .context("full dream (cycle + compile)")?;

    println!("rem cycle      : {}", report.cycle.cycle_id);
    println!(
        "duration       : {} ms",
        (report.cycle.ended_at - report.cycle.started_at).num_milliseconds()
    );
    println!(
        "auto_apply     : applied={}",
        report.cycle.auto_apply.applied.len()
    );
    println!(
        "revisor        : examined={} confirmed={} dedup_applied={}",
        report.cycle.revisor.pairs_examined,
        report.cycle.revisor.pairs_confirmed,
        report.cycle.revisor.applied.len(),
    );
    println!(
        "auto_promote   : candidates={} proposals={}",
        report.cycle.auto_promote.candidates_examined,
        report.cycle.auto_promote.applied.len(),
    );
    println!(
        "archive        : paths={} proposals={}",
        report.cycle.archive_detector.paths_examined,
        report.cycle.archive_detector.proposals_emitted.len(),
    );
    println!(
        "hub_writer     : regenerated={}",
        report.cycle.hub_writer.regenerated.len()
    );
    println!(
        "compile        : leaves={} lists={} hubs={} unchanged={} errors={}",
        report.compile.leaves,
        report.compile.lists,
        report.compile.hubs,
        report.compile.unchanged,
        report.compile.errors.len(),
    );
    drop(lock);
    Ok(())
}

/// Run one light dream from the CLI: promote buffered captures into
/// `fact_index`, then — when the LLM slots (incl. `cronista`) are configured —
/// compile the pages the promotion dirtied. Promotion is deterministic; the LLM
/// bag is built best-effort from config and is optional (without it, only the
/// promotion runs). Acquires the workdir lockfile so it never races with a
/// running `mwe-mcp serve`.
async fn cmd_rem_run_light(workdir: &Path, config: &Config) -> Result<()> {
    info!(workdir = %workdir.display(), "mwe-mcp rem run-light: starting");

    let lock = lockfile::acquire(workdir)
        .map_err(|e| anyhow!("lockfile: {e} (is `mwe-mcp serve` already running?)"))?;
    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;
    let tree = WikiTree::open(workdir).context("opening wikis/ tree")?;
    let embedder: Arc<dyn Embedder> = config
        .embedding
        .build_embedder()
        .await
        .context("building embedder")?;
    // Optional: when `hub_writer` + `rem_dedup_semantic` (+ `cronista`) are
    // configured, the light dream also compiles the pages the promotion
    // dirtied. Absent config ⇒ `None` ⇒ promotion only.
    let llms = rem_scheduler::build_backends(&config.llm)?;
    let policy = mwe_core::dream_light::LightPolicy::default();
    let report = rem_scheduler::run_light_once(&pool, &tree, embedder, llms.as_ref(), &policy)
        .await
        .context("light dream cycle")?;

    println!(
        "light dream    : scanned={} promoted={} skipped_dup={} superseded={} errors={}",
        report.light.scanned,
        report.light.promoted,
        report.light.skipped_dup,
        report.light.superseded,
        report.light.errors.len(),
    );
    if let Some(compile) = &report.compile {
        println!(
            "compile        : leaves={} lists={} hubs={} unchanged={} errors={}",
            compile.leaves,
            compile.lists,
            compile.hubs,
            compile.unchanged,
            compile.errors.len(),
        );
    }
    for e in &report.light.errors {
        eprintln!("  soft error: {e}");
    }
    drop(lock);
    Ok(())
}

/// Run one narrative compile pass from the CLI: rebuild the plan and
/// compile the dirty pages into prose. Needs the `cronista` (+ `hub_writer`)
/// slots; lockfile-guarded.
async fn cmd_rem_run_compile(workdir: &Path, config: &Config) -> Result<()> {
    info!(workdir = %workdir.display(), "mwe-mcp rem run-compile: starting");

    let lock = lockfile::acquire(workdir)
        .map_err(|e| anyhow!("lockfile: {e} (is `mwe-mcp serve` already running?)"))?;
    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;
    let tree = WikiTree::open(workdir).context("opening wikis/ tree")?;
    let llms = rem_scheduler::build_backends(&config.llm)?.ok_or_else(|| {
        anyhow!(
            "rem run-compile: `llm.hub_writer` or `llm.rem_dedup_semantic` are not configured; \
             configure them (and `llm.cronista` for the prose writer) to compile"
        )
    })?;
    let report =
        rem_scheduler::run_compile_once(&pool, &tree, &llms, &chrono::Utc::now().to_rfc3339())
            .await
            .context("compile pass")?;

    println!(
        "compile        : leaves={} lists={} hubs={} unchanged={} errors={}",
        report.leaves,
        report.lists,
        report.hubs,
        report.unchanged,
        report.errors.len(),
    );
    for e in &report.errors {
        eprintln!("  soft error: {e}");
    }
    drop(lock);
    Ok(())
}

/// `mwe-mcp recall eval` — replay a gold set and print the scoreboard:
/// flat-RAG baseline (hit@1 / hit@3 / coverage) vs recall-as-navigation
/// (coverage + deviating catches). Read-only by design: **no lockfile**
/// (it may run next to a live `mwe-mcp serve`) and **no recall-counter
/// bumps** (the harness uses the unrecorded search variant).
async fn cmd_recall_eval(
    workdir: &Path,
    config: &Config,
    gold_path: &Path,
    flat_only: bool,
) -> Result<()> {
    let yaml = std::fs::read_to_string(gold_path)
        .with_context(|| format!("reading gold file {}", gold_path.display()))?;
    let gold = mwe_core::recall_eval::GoldSet::parse(&yaml).context("parsing gold YAML")?;
    if gold.queries.is_empty() {
        bail!("gold file holds no queries");
    }

    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;
    let tree = WikiTree::open(workdir).context("opening wikis/ tree")?;
    let embedder: Arc<dyn Embedder> = config
        .embedding
        .build_embedder()
        .await
        .context("building embedder")?;

    // The navigator backend mirrors the ingest call sites: optional —
    // a missing/unbuildable slot measures the flat baseline only.
    let navigator = if flat_only {
        println!("navigation     : skipped (--flat-only)");
        None
    } else {
        match config
            .llm
            .slot(mwe_core::config::LlmFunction::Navigator)
            .map(|slot| slot.build_backend(mwe_core::config::LlmFunction::Navigator))
        {
            Some(Ok(backend)) => Some(backend),
            Some(Err(e)) => {
                println!("navigation     : skipped (navigator backend failed to build: {e})");
                None
            },
            None => {
                println!("navigation     : skipped (llm.navigator not configured)");
                None
            },
        }
    };

    let policy = config.recall.resolved_ingest_policy();
    let report = mwe_core::recall_eval::run_eval(
        &pool,
        &tree,
        embedder,
        navigator.as_deref(),
        &gold,
        &policy,
    )
    .await
    .context("recall eval")?;

    for q in &report.queries {
        let nav = q.nav.as_ref().map_or_else(
            || "nav -".to_owned(),
            |n| {
                format!(
                    "nav {}/{} (deviating {}, {} pages, {} hops)",
                    n.covered, q.expectations, n.deviating, n.fragments, n.hops
                )
            },
        );
        println!(
            "{:<32} hit@1={} hit@3={} · flat {}/{} · {} · missing {}",
            q.label,
            u8::from(q.flat_hit_at_1),
            u8::from(q.flat_hit_at_3),
            q.flat_covered,
            q.expectations,
            nav,
            q.missing.len(),
        );
        for m in &q.missing {
            println!("{:<32}   missing: {m}", "");
        }
    }
    let pct = |r: f64| format!("{:.0}%", r * 100.0);
    println!(
        "aggregate      : queries={} · hit@1 {} · hit@3 {} · coverage flat {} / nav {} / combined {} · deviating found {}",
        report.queries.len(),
        pct(report.hit_at_1_rate()),
        pct(report.hit_at_3_rate()),
        pct(report.flat_coverage()),
        report.nav_coverage().map_or_else(|| "-".to_owned(), pct),
        pct(report.combined_coverage()),
        report.deviating_found(),
    );
    Ok(())
}

/// HTTP transport. Same Axum process binds `/dashboard/*` (built-in
/// web UI, cookie auth) and `/mcp` (rmcp Streamable HTTP, JWT auth).
/// Defence-in-depth: the on-disk wiki bytes are cleartext, so per-reader ACL
/// is only a real boundary when the OS keeps non-server principals out of the
/// workdir. Warn loudly (never fatal) for each workdir path reachable by group
/// or world — a co-located consumer reading the files would bypass the
/// governance. See `workdir_security` and INTEGRATING.md "Deployment security".
fn warn_loose_workdir(workdir: &Path) {
    for f in workdir_security::audit(workdir) {
        warn!(
            path = %f.path.display(),
            mode = format!("{:#o}", f.mode),
            severity = f.severity.tag(),
            "workdir permissions: reachable by another principal — the on-disk wiki bytes are \
             cleartext, so a co-located process bypasses per-reader ACL; restrict the workdir \
             (`{}`) or run the consumer on a separate host/user (INTEGRATING.md \
             \"Deployment security\")",
            workdir_security::remediation(workdir),
        );
    }
}

/// The dedicated service account the production gate provisions and runs under.
const DEDICATED_USER: &str = "mwe-mcp";
/// Home of the dedicated account (created with `--create-home`); its `workdir/`
/// subdirectory holds the deployment.
const DEDICATED_HOME: &str = "/home/mwe-mcp";
/// Canonical production workdir, owned by [`DEDICATED_USER`] and `chmod 700`.
const DEDICATED_WORKDIR: &str = "/home/mwe-mcp/workdir";
/// Where the production binary is installed so the service user can exec it: a
/// binary under the operator's `~/.local/bin` is typically unreadable to the
/// dedicated account.
const PROD_BIN: &str = "/usr/local/bin/mwe-mcp";
/// The systemd unit the desktop tray watches and `serve` provisions.
const SERVICE_UNIT_PATH: &str = "/etc/systemd/system/mwe-mcp.service";

/// Loopback default — the server is reachable only from this machine.
const DEFAULT_BIND: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
/// The project's conventional listen port.
const DEFAULT_PORT: u16 = 8742;
/// All interfaces — reachable from other machines (LAN / port-forwardable).
const EXPOSED_BIND: IpAddr = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);

/// Resolve the listen address. Explicit `--bind`/`--port` always win. On a bare
/// interactive `serve` (neither flag given, a real terminal both ends) it asks
/// whether to expose the server to other machines and on which port — mwe-mcp is
/// a server multiple consumers connect to over HTTP, frequently from other hosts
/// (Claude Code on another PC, the claude.ai web chat, a phone bot). Otherwise it
/// resolves silently to the loopback defaults.
fn resolve_exposure(bind: Option<IpAddr>, port: Option<u16>) -> (IpAddr, u16) {
    let interactive = bind.is_none()
        && port.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    if interactive {
        prompt_exposure()
    } else {
        (bind.unwrap_or(DEFAULT_BIND), port.unwrap_or(DEFAULT_PORT))
    }
}

/// Interactively choose the listen address (the bare-`serve` exposure prompt).
/// Any read failure falls back to the safe loopback defaults.
fn prompt_exposure() -> (IpAddr, u16) {
    eprintln!(
        "\n\
         mwe-mcp is a server your AI agents reach over HTTP — they may live on this \
         machine or on others (Claude Code on another PC, the claude.ai web chat, a \
         phone bot). The bind address decides who can reach it."
    );
    eprint!(
        "Expose it to other machines? y = bind 0.0.0.0 (reachable on your LAN / \
         port-forwardable); N = 127.0.0.1, this machine only [y/N] "
    );
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    let expose = std::io::stdin().read_line(&mut line).is_ok()
        && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    let bind = if expose {
        eprintln!(
            "  → binding 0.0.0.0. The endpoint is JWT-gated but plain HTTP, so for anything \
             past a trusted LAN put TLS in front (a reverse proxy or a tunnel such as \
             Cloudflare Tunnel) and mint `exposed` (30-day) tokens; open / forward the port \
             on your router as needed. On a box dedicated to mwe-mcp (no consumer agent \
             alongside it) you'll also want --bypassdedicateduser — the dedicated-user \
             boundary only matters when an agent shares this machine."
        );
        EXPOSED_BIND
    } else {
        DEFAULT_BIND
    };

    eprint!("Port [{DEFAULT_PORT}]: ");
    let _ = std::io::stderr().flush();
    let mut pline = String::new();
    let port = if std::io::stdin().read_line(&mut pline).is_ok() {
        let trimmed = pline.trim();
        if trimmed.is_empty() {
            DEFAULT_PORT
        } else {
            trimmed.parse().unwrap_or_else(|_| {
                eprintln!("  → `{trimmed}` is not a valid port; using {DEFAULT_PORT}.");
                DEFAULT_PORT
            })
        }
    } else {
        DEFAULT_PORT
    };
    (bind, port)
}

/// What the dedicated-user gate decided the caller should do.
#[derive(Debug, PartialEq, Eq)]
enum GateOutcome {
    /// Boot the MCP server in this foreground process.
    Boot,
    /// The deployment runs as a systemd service that owns the port; the
    /// foreground command must return without binding a second listener.
    HandedToService,
}

/// Dedicated-user startup gate (roadmap 14b/14c). `warn_loose_workdir` catches
/// *other* users reaching the workdir; this catches the same-user case 0700
/// cannot — a co-located agent running as the same login user reads the
/// cleartext bytes regardless.
///
/// Running as a dedicated (nologin) account, or with `--bypassdedicateduser`,
/// boots in place. As root or a login-capable account the boundary is real: on
/// an interactive terminal `serve` offers to provision the systemd service (see
/// [`offer_and_setup_dedicated_service`]) and, once it is running, hands the
/// port to it; non-interactively (systemd, CI, container, piped) it keeps the
/// loud, actionable refusal.
fn enforce_dedicated_user(
    workdir: &Path,
    bind: IpAddr,
    port: u16,
    bypass: bool,
) -> Result<GateOutcome> {
    let class = workdir_security::classify_current_user();
    if bypass {
        if class.is_dedicated() {
            return Ok(GateOutcome::Boot);
        }
        // login or root: the co-location boundary is deliberately not enforced.
        warn!(
            "--bypassdedicateduser: the dedicated-user gate is disabled — the co-location \
             trust boundary is NOT enforced; a process running as this same user can read the \
             cleartext workdir and bypass the per-reader ACL"
        );
        // A host dedicated to mwe-mcp still wants a restart-on-boot service.
        // Offer one (running as this login user, carrying the bypass) when
        // interactive and not already installed; otherwise just run foreground.
        if let UserClass::LoginAccount { user, .. } = &class
            && !Path::new(SERVICE_UNIT_PATH).exists()
            && std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && offer_and_setup_bypass_service(workdir, bind, port, user)?
        {
            return Ok(GateOutcome::HandedToService);
        }
        return Ok(GateOutcome::Boot);
    }
    let running_as = match class {
        UserClass::Dedicated { user } => {
            info!(%user, "dedicated-user gate: ok");
            return Ok(GateOutcome::Boot);
        },
        UserClass::Root => "root (uid 0)".to_owned(),
        UserClass::LoginAccount { user, shell } => {
            format!("the login account `{user}` (shell {shell})")
        },
    };

    // A login/root account: the co-location boundary actually bites here. If the
    // service is already installed, this foreground invocation is redundant —
    // point the operator at the running service rather than re-provisioning.
    if Path::new(SERVICE_UNIT_PATH).exists() {
        eprintln!("{}", service_already_installed());
        return Ok(GateOutcome::HandedToService);
    }

    // Interactive terminal → offer the one-shot service setup; if the operator
    // accepts and it succeeds, the service now owns the port. Declined or
    // non-interactive falls through to the refusal.
    if std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && offer_and_setup_dedicated_service(workdir, bind, port, &running_as)?
    {
        return Ok(GateOutcome::HandedToService);
    }
    bail!(dedicated_user_refusal(workdir, &running_as));
}

/// Render the `mwe-mcp.service` systemd unit. Pure (no I/O) so it is unit-test
/// covered. `user` is the account the service runs as; `bypass` appends
/// `--bypassdedicateduser` to `ExecStart` — set for a service that runs as a
/// login account on a host dedicated to mwe-mcp, cleared for the `mwe-mcp`
/// service account (which passes the gate on its own). `XDG_CACHE_HOME` is
/// pinned inside the workdir so the bge-m3 weights (~2.2 GB, fetched on first
/// run) land on a `ReadWritePaths` path — `ProtectSystem=strict` makes the rest
/// of the filesystem read-only, and the cache otherwise defaults to
/// `$HOME/.cache`, outside the workdir.
fn service_unit(
    bin: &str,
    workdir: &str,
    bind: IpAddr,
    port: u16,
    user: &str,
    bypass: bool,
) -> String {
    let exec_bypass = if bypass { " --bypassdedicateduser" } else { "" };
    format!(
        "[Unit]\n\
         Description=mwe-mcp — agent-agnostic memory MCP server\n\
         Documentation=https://github.com/Fr4nZ82/mwe-mcp\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         WorkingDirectory={workdir}\n\
         Environment=XDG_CACHE_HOME={workdir}/.cache\n\
         ExecStart={bin} serve --workdir {workdir} --bind {bind} --port {port}{exec_bypass}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         NoNewPrivileges=true\n\
         ProtectSystem=strict\n\
         ReadWritePaths={workdir}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Run a privileged setup step via `sudo` (prompting on the interactive
/// terminal that already gated us here). Errors carry the failed command and
/// the `--bypassdedicateduser` escape hatch.
fn run_privileged(action: &str, argv: &[&str]) -> Result<()> {
    let shown = argv.join(" ");
    info!(command = %shown, "dedicated-user setup: sudo {shown}");
    let status = std::process::Command::new("sudo")
        .args(argv)
        .status()
        .with_context(|| format!("failed to spawn `sudo {shown}` (to {action})"))?;
    if !status.success() {
        bail!(
            "`sudo {shown}` failed while trying to {action} ({status}). Fix the cause and re-run \
             `mwe-mcp serve`, or pass --bypassdedicateduser to start a throwaway server as \
             yourself."
        );
    }
    Ok(())
}

/// Whether an OS account named `user` already exists (idempotent `useradd`).
fn user_exists(user: &str) -> bool {
    std::process::Command::new("id")
        .arg(user)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Install the production binary to [`PROD_BIN`], place the rendered unit at
/// [`SERVICE_UNIT_PATH`], and enable + start it. Shared by both service-setup
/// paths (the dedicated account and the login-account bypass).
fn install_and_start_service(exe: &str, unit: &str) -> Result<()> {
    run_privileged(
        "install the production binary",
        &["install", "-m", "0755", exe, PROD_BIN],
    )?;
    // Stage the unit as the current user, then place it with sudo.
    let staged = std::env::temp_dir().join(format!("mwe-mcp.service.{}", std::process::id()));
    std::fs::write(&staged, unit).context("stage the systemd unit file")?;
    let staged_s = staged.to_string_lossy().into_owned();
    let placed = run_privileged(
        "install the systemd unit",
        &["install", "-m", "0644", &staged_s, SERVICE_UNIT_PATH],
    );
    let _ = std::fs::remove_file(&staged);
    placed?;
    run_privileged("reload systemd", &["systemctl", "daemon-reload"])?;
    run_privileged(
        "enable and start the service",
        &["systemctl", "enable", "--now", "mwe-mcp.service"],
    )?;
    Ok(())
}

/// Interactively provision the dedicated-user systemd service. Prints the exact
/// privileged commands, asks for confirmation, and on "yes" creates the account,
/// installs the production binary, relocates (preserving data) or creates and
/// locks the workdir, installs the unit, and enables + starts the service.
///
/// Returns `Ok(true)` when the service is running, `Ok(false)` when the operator
/// declined, and `Err` when a step failed.
fn offer_and_setup_dedicated_service(
    workdir: &Path,
    bind: IpAddr,
    port: u16,
    running_as: &str,
) -> Result<bool> {
    let exe = std::env::current_exe().context("resolve the running mwe-mcp binary path")?;
    let exe_s = exe.to_string_lossy().into_owned();
    let wd = workdir.display();

    eprintln!(
        "\n\
         mwe-mcp serve is running as {running_as}.\n\
         The on-disk workdir is cleartext, so any process this same user runs — including a \
         co-located agent's file tool — can read every memory-wiki fragment, bypassing the \
         per-reader ACL. The fix is to run mwe-mcp as a dedicated service account.\n\n\
         I can set that up now. With sudo, this will:\n\n  \
         useradd --system --create-home --home-dir {DEDICATED_HOME} --shell /usr/sbin/nologin {DEDICATED_USER}\n  \
         install -m 0755 {exe_s} {PROD_BIN}\n  \
         mv {wd} {DEDICATED_WORKDIR}   (or create it fresh if it does not exist yet)\n  \
         chown -R {DEDICATED_USER}:{DEDICATED_USER} {DEDICATED_WORKDIR} && chmod 700 {DEDICATED_WORKDIR}\n  \
         install the systemd unit at {SERVICE_UNIT_PATH}\n  \
         systemctl daemon-reload && systemctl enable --now mwe-mcp.service\n\n\
         The service runs as {DEDICATED_USER}, restarts on failure, and starts on boot.\n"
    );
    eprint!("Proceed? [y/N] ");
    std::io::stderr().flush().ok();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("read confirmation from stdin")?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer != "y" && answer != "yes" {
        return Ok(false);
    }

    // 1. Dedicated account — idempotent (reuse an existing one).
    if user_exists(DEDICATED_USER) {
        info!(
            user = DEDICATED_USER,
            "dedicated-user setup: account exists, reusing"
        );
    } else {
        run_privileged(
            "create the dedicated service account",
            &[
                "useradd",
                "--system",
                "--create-home",
                "--home-dir",
                DEDICATED_HOME,
                "--shell",
                "/usr/sbin/nologin",
                DEDICATED_USER,
            ],
        )?;
    }

    // 2. Relocate (preserving any data) or create the workdir, then lock it down.
    let current = std::fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());
    let current_s = current.to_string_lossy().into_owned();
    if current != Path::new(DEDICATED_WORKDIR) {
        if current.exists() {
            run_privileged(
                "move the existing workdir under the dedicated account",
                &["mv", &current_s, DEDICATED_WORKDIR],
            )?;
        } else {
            run_privileged(
                "create the dedicated workdir",
                &["install", "-d", "-m", "700", DEDICATED_WORKDIR],
            )?;
        }
    }
    let owner = format!("{DEDICATED_USER}:{DEDICATED_USER}");
    run_privileged(
        "give the dedicated account ownership of the workdir",
        &["chown", "-R", &owner, DEDICATED_WORKDIR],
    )?;
    run_privileged(
        "lock the workdir to its owner",
        &["chmod", "700", DEDICATED_WORKDIR],
    )?;

    // 3. Install the binary + unit and enable the service.
    install_and_start_service(
        &exe_s,
        &service_unit(
            PROD_BIN,
            DEDICATED_WORKDIR,
            bind,
            port,
            DEDICATED_USER,
            false,
        ),
    )?;

    eprintln!(
        "\n\
         ✓ mwe-mcp.service is running as {DEDICATED_USER}.\n  \
         Dashboard: http://{bind}:{port}/dashboard\n  \
         Logs:      journalctl -u mwe-mcp -f\n  \
         Manage:    sudo systemctl status|restart|stop mwe-mcp   (or the desktop tray)\n"
    );
    Ok(true)
}

/// Interactively offer to install mwe-mcp as a restart-on-boot systemd service
/// **running as the current login user** with `--bypassdedicateduser` — the
/// shape for a host dedicated to mwe-mcp where the operator opted out of the
/// dedicated-user boundary (no co-located consumer to wall off). The workdir is
/// not relocated (it is already owned by this user). Returns `Ok(true)` when the
/// service is running, `Ok(false)` when declined.
fn offer_and_setup_bypass_service(
    workdir: &Path,
    bind: IpAddr,
    port: u16,
    user: &str,
) -> Result<bool> {
    let exe = std::env::current_exe().context("resolve the running mwe-mcp binary path")?;
    let exe_s = exe.to_string_lossy().into_owned();
    // systemd needs an absolute, existing WorkingDirectory; the workdir stays
    // where it is (already owned by this same user — no relocation, no chown).
    let wd_abs = if workdir.is_absolute() {
        workdir.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory for the workdir")?
            .join(workdir)
    };
    std::fs::create_dir_all(&wd_abs)
        .with_context(|| format!("create the workdir {} for the service", wd_abs.display()))?;
    let wd_abs = std::fs::canonicalize(&wd_abs).unwrap_or(wd_abs);
    let wd_s = wd_abs.to_string_lossy().into_owned();

    eprintln!(
        "\n\
         Install mwe-mcp as a systemd service, so it restarts on failure and starts on \
         boot? It will run as you (`{user}`) with --bypassdedicateduser, serving on \
         {bind}:{port}. With sudo, this will:\n\n  \
         install -m 0755 {exe_s} {PROD_BIN}\n  \
         install the systemd unit at {SERVICE_UNIT_PATH} (User={user})\n  \
         systemctl daemon-reload && systemctl enable --now mwe-mcp.service\n"
    );
    eprint!("Proceed? [y/N] ");
    std::io::stderr().flush().ok();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("read confirmation from stdin")?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(false);
    }

    install_and_start_service(
        &exe_s,
        &service_unit(PROD_BIN, &wd_s, bind, port, user, true),
    )?;

    eprintln!(
        "\n\
         ✓ mwe-mcp.service is running as {user}.\n  \
         Dashboard: http://{bind}:{port}/dashboard\n  \
         Logs:      journalctl -u mwe-mcp -f\n  \
         Manage:    sudo systemctl status|restart|stop mwe-mcp\n"
    );
    Ok(true)
}

/// Message shown when `mwe-mcp.service` is already installed and `serve` is run
/// again from a login account — manage the running service instead of starting
/// a second foreground listener.
fn service_already_installed() -> String {
    format!(
        "mwe-mcp.service is already installed at {SERVICE_UNIT_PATH}; the deployment runs as the \
         dedicated `{DEDICATED_USER}` account.\n\
         Not starting a second foreground server. Manage the service with:\n\n  \
         sudo systemctl status mwe-mcp     # is it running?\n  \
         sudo systemctl restart mwe-mcp    # apply changes / restart\n  \
         journalctl -u mwe-mcp -f          # logs\n\n\
         To run a throwaway foreground server as yourself instead, pass --bypassdedicateduser."
    )
}

/// The loud, actionable refusal for the dedicated-user gate — shown when the
/// interactive setup is declined or unavailable (non-interactive host).
fn dedicated_user_refusal(workdir: &Path, running_as: &str) -> String {
    format!(
        "refusing to start: mwe-mcp serve is running as {running_as}.\n\
         The on-disk workdir is cleartext, so any process this same user runs — including a \
         co-located agent's file tool — can read every memory-wiki fragment, bypassing the \
         per-reader ACL.\n\n\
         Run the server under a dedicated service account instead:\n\n  \
         sudo useradd --system --create-home --home-dir {DEDICATED_HOME} --shell /usr/sbin/nologin {DEDICATED_USER}\n  \
         sudo install -m 0755 <this-binary> {PROD_BIN}\n  \
         sudo mv {wd} {DEDICATED_WORKDIR} && sudo chown -R {DEDICATED_USER}:{DEDICATED_USER} {DEDICATED_WORKDIR}\n  \
         sudo chmod 700 {DEDICATED_WORKDIR}\n  \
         sudo install -m 0644 <unit> {SERVICE_UNIT_PATH} && sudo systemctl enable --now mwe-mcp.service\n\n\
         In an interactive terminal, `mwe-mcp serve` offers to do all of this for you.\n\
         If a dedicated user is genuinely impossible (some managed/remote hosts, containers), pass \
         --bypassdedicateduser to start anyway — the co-location boundary will then NOT be enforced.",
        wd = workdir.display(),
    )
}

// A linear bootstrap: resolve exposure, enforce the dedicated-user gate,
// assemble the router tree (each `.nest`/`.merge` carries a why-comment),
// bind, and serve with graceful shutdown. Splitting it would scatter that
// single startup narrative; the body is just over the 100-line lint after
// stable's line-counting drifted (101/100).
#[allow(
    clippy::too_many_lines,
    reason = "linear server bootstrap; see comment above"
)]
async fn cmd_serve_http(
    workdir: &Path,
    config: &Config,
    bind: Option<IpAddr>,
    port: Option<u16>,
    bypassdedicateduser: bool,
) -> Result<()> {
    // Resolve the listen address first — explicit flags win; a bare interactive
    // `serve` asks whether to expose the server to other machines — so the
    // chosen bind/port also bake into the systemd unit if the dedicated-user
    // gate provisions one below.
    let (bind, port) = resolve_exposure(bind, port);
    info!(workdir = %workdir.display(), %bind, port, transport = "http", "mwe-mcp serve: starting");

    // Production trust boundary (roadmap 14b/14c): boot only under a dedicated
    // account (or explicit opt-out). On a login/root account with a terminal,
    // this offers to provision the systemd service; once it owns the port the
    // foreground command returns without binding a second listener.
    match enforce_dedicated_user(workdir, bind, port, bypassdedicateduser)? {
        GateOutcome::Boot => {},
        GateOutcome::HandedToService => return Ok(()),
    }
    // Advisory perms check — fires in the bypass case and for a dedicated user
    // whose workdir is still group/world-reachable.
    warn_loose_workdir(workdir);

    let (state, dashboard_state) = bootstrap_state(workdir, config).await?;

    // One broadcast channel fans the shutdown signal out to every
    // long-lived task that needs to exit cleanly: axum's graceful
    // shutdown future, the schedulers, and the workers. Capacity 1 is
    // enough — the signal fires once, and each subscriber wakes up on
    // its own copy independently of the others. Created before the
    // router assembly because the dashboard's Backup console carries a
    // restart handle wired to the same channel (a staged recovery needs
    // a boot to apply).
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let ctrl_c_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            warn!("failed to install ctrl-c handler; serving without graceful shutdown");
            return;
        }
        info!("mwe-mcp serve: ctrl-c received, broadcasting shutdown");
        let _ = ctrl_c_tx.send(());
    });
    let restart_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dashboard_state = dashboard_state.with_restart(mwe_dashboard::RestartHandle {
        shutdown: shutdown_tx.clone(),
        requested: restart_requested.clone(),
    });

    // Shared REM policy handle: the same `Arc` the dashboard state (REM
    // settings editor + Dream console) holds, cloned out before the
    // router assembly consumes `dashboard_state`, so the scheduler below
    // reads a settings save at its next cycle start — no restart.
    let rem_policy = dashboard_state.rem_policy.clone();
    // Same for the backup schedule (Backup console ↔ backup scheduler).
    let backup_schedule = dashboard_state.backup_schedule.clone();

    let mcp_state_for_router = state.clone();
    let mcp_state_for_factory = Arc::new(state.clone());
    let session_manager = Arc::new(LocalSessionManager::default());
    // Stateless mode keeps the dispatcher simple: every POST is a fresh
    // session, no SSE re-priming, JSON response so consumers without an
    // SSE client get the plain body.
    //
    // `disable_allowed_hosts()` turns off rmcp's default Host allow-list
    // (loopback only), which otherwise 403s every request whose `Host` is the
    // operator's public hostname behind a tunnel/reverse proxy (e.g. a
    // Cloudflare Tunnel, or the claude.ai web connector) — see the
    // `webagentoauth` flow. The DNS-rebinding protection that allow-list
    // provides is **redundant here**: `/mcp` is gated by a `Authorization:
    // Bearer <jwt>` (the `jwt_auth_middleware` layer below), which a
    // browser-driven DNS-rebinding attack cannot forge (unlike a cookie), so
    // the JWT — not the Host — is the security boundary. A fixed host list
    // would also break every deployment whose public hostname we cannot know
    // at build time. `allowed_origins` stays at its default (empty ⇒ any),
    // which is already permissive.
    let streamable_config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .disable_allowed_hosts();
    let factory = mcp::factory_for(mcp_state_for_factory);
    let streamable = StreamableHttpService::new(factory, session_manager, streamable_config);

    let mcp_router = Router::new()
        .fallback_service(streamable)
        .layer(axum::middleware::from_fn_with_state(
            mcp_state_for_router.clone(),
            mcp::auth::jwt_auth_middleware,
        ))
        // Outermost layer so every /mcp response — auth rejections
        // included — carries an explicit charset.
        .layer(axum::middleware::from_fn(mcp_utf8_charset))
        .with_state(mcp_state_for_router);

    let app = Router::new()
        .nest("/dashboard", mwe_dashboard::router(dashboard_state.clone()))
        // Canonical short form of the citation-handle
        // resolver. The same handler is also reachable as
        // `/dashboard/cite/:bi_id` (alias inside the dashboard router);
        // mounting at the root keeps the URL short and copy-pasteable
        // in agent responses. Anonymous on purpose — auth fires on the
        // destination `/dashboard/wiki/...` page.
        .merge(mwe_dashboard::cite_router(dashboard_state.clone()))
        // Inbound OAuth 2.x authorization server (`webagentoauth`, roadmap 19):
        // discovery (`/.well-known/oauth-*`), Dynamic Client Registration and the
        // token endpoint, mounted at the root so a remote MCP client (the
        // claude.ai web app) can run the OAuth dance with no hand-copied token.
        // The consent step that needs a login is `/dashboard/webagentoauth/authorize`
        // (in the dashboard router). Anonymous like the other root surfaces — the
        // human login + consent is the gate, not client identity.
        .merge(mwe_dashboard::webagentoauth_public_router(dashboard_state))
        .nest("/mcp", mcp_router)
        // The media byte pair (upload + ACL-enforced serving), behind
        // the same bearer JWT as /mcp — the MCP ingest stays JSON and
        // bytes travel out of band here. The dashboard renders embeds
        // through its own cookie-authenticated alias.
        .nest("/media", mwe_mcp_server::http_media::router(state.clone()))
        // Public read of bundled skills. Custom skills
        // stay MCP-only (`skill_list` / `skill_fetch`) since they are
        // owner-scoped and the HTTP path has no JWT context.
        .nest("/skills", mwe_mcp_server::http_skills::router())
        // Operator-facing onboarding surface. Today
        // ships the hook bundle templates (`/connect/hooks` +
        // `/connect/hooks/<consumer>.json`). The full `/connect`
        // landing page (URL + one-shot token + per-consumer links)
        // arrives later alongside the dashboard UI.
        .nest("/connect", mwe_mcp_server::http_connect::router())
        // Public, anonymous bridge-distribution surface mounted at the
        // root: the slim front page (`/`, an agent line + a human
        // sign-in), the bridge catalog (`/bridges`), and the
        // self-contained installers (`/bridges/<consumer>/install.{sh,ps1,md}`).
        // Unauthenticated on purpose — `curl … | sh` reaches it from a
        // box with no dashboard session, and nothing here is secret (the
        // token is issued from the dashboard home, never here). The same
        // catalog is also the authenticated `/dashboard/bridges` tab.
        .merge(mwe_dashboard::public_site_router());

    let addr = SocketAddr::new(bind, port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    // Runtime housekeeping: drain the residue the inline paths cannot
    // reach retroactively — expired authorization codes, stale refresh
    // rows, web-agent consumers whose smart wiki was deleted.
    match mwe_core::housekeeping::run(&state.pool, &state.tree).await {
        Ok(report) if report.is_noop() => {},
        Ok(report) => info!(
            auth_codes_purged = report.auth_codes_purged,
            stale_refresh_pruned = report.stale_refresh_pruned,
            dangling_consumers_removed = report.dangling_consumers_removed,
            delegations_removed = report.delegations_removed,
            "boot housekeeping: swept"
        ),
        Err(error) => warn!(%error, "boot housekeeping failed; serving anyway"),
    }

    // REM scheduler. Built before
    // the listener starts accepting traffic so a misconfigured
    // `llm.hub_writer` / `llm.rem_dedup_semantic` slot surfaces in the
    // startup log instead of half an hour later on the first tick.
    // Build the LLM bag once and share it (Arc) between the REM
    // full cycle and the light dream — both run the narrative compile pass.
    let llms = rem_scheduler::build_backends(&config.llm)
        .context("building REM LLM backends")?
        .map(std::sync::Arc::new);
    if llms.is_none() {
        warn!(
            "rem scheduler: `llm.hub_writer` or `llm.rem_dedup_semantic` not configured — \
             the REM full cycle will not auto-run (configure both or use `mwe-mcp rem run-cycle`); \
             the light dream still promotes captures but cannot compile prose without the slots"
        );
    }
    let scheduler_handle = if let Some(llms) = &llms {
        let mut rx = shutdown_tx.subscribe();
        rem_scheduler::spawn(
            config.rem.schedule,
            state.pool.clone(),
            state.tree.clone(),
            state.embedder.clone(),
            std::sync::Arc::clone(llms),
            rem_policy,
            async move {
                let _ = rx.recv().await;
            },
        )
    } else {
        None
    };

    // Light dream: the frequent captures→facts promotion loop.
    // Promotion is deterministic, so it runs even without the LLM bag; the
    // compile step is skipped when `llms` is None.
    let mut light_shutdown_rx = shutdown_tx.subscribe();
    let light_handle = rem_scheduler::spawn_light(
        config.rem.schedule,
        state.pool.clone(),
        state.tree.clone(),
        state.embedder.clone(),
        llms.clone(),
        async move {
            let _ = light_shutdown_rx.recv().await;
        },
    );

    // Backup scheduler: the automatic-snapshot due-check loop (`backup:`
    // config section). Always armed — a disabled schedule idles, and the
    // Backup console can enable it without a restart.
    let mut backup_shutdown_rx = shutdown_tx.subscribe();
    let backup_handle = backup_scheduler::spawn(
        config.backup.initial_delay_secs,
        backup_schedule,
        state.pool.clone(),
        workdir.to_path_buf(),
        async move {
            let _ = backup_shutdown_rx.recv().await;
        },
    );

    // Document worker: drives queued `wiki_ingest_external` jobs
    // (classify → segment → anchor → extract → reduce → file). Runs on
    // the `ingest` slot, built per tick; without the slot the loop idles
    // (enqueue already refuses, so the queue only holds runnable jobs).
    let mut document_shutdown_rx = shutdown_tx.subscribe();
    let document_handle = tokio::spawn(mwe_core::document::run_worker_loop(
        state.pool.clone(),
        state.tree.clone(),
        state.embedder.clone(),
        config.llm.clone(),
        workdir.to_path_buf(),
        config.document.resolved_policy(),
        async move {
            let _ = document_shutdown_rx.recv().await;
        },
    ));

    println!("mwe-mcp serve: ready (transport=http)");
    println!("workdir : {}", workdir.display());
    println!("listen  : http://{addr}");
    println!("routes  : /dashboard/* (web UI, cookie auth)");
    println!("         /mcp* (rmcp Streamable HTTP, Bearer auth)");
    println!();
    println!("First-run setup wizard at: http://{addr}/dashboard/setup");
    println!("Stop with Ctrl-C.");

    // Trim trailing slashes before routing so `/dashboard/` (and any
    // other directory-like URL a user might type or bookmark) resolves
    // identically to `/dashboard`. Without this, `axum::Router::nest`
    // matches `/dashboard` but rejects `/dashboard/` with a bare 404 —
    // the brand link in the dashboard layout points at the trailing-
    // slash form, so the affordance to "go home" was dead on arrival.
    let app = {
        use tower::Layer;
        tower_http::normalize_path::NormalizePathLayer::trim_trailing_slash().layer(app)
    };
    // `axum::serve` wants a `MakeService`; bring `axum::ServiceExt`
    // into scope only for the duration of this block.
    let make_service = {
        use axum::ServiceExt;
        ServiceExt::<axum::extract::Request>::into_make_service(app)
    };

    let mut axum_shutdown_rx = shutdown_tx.subscribe();
    axum::serve(listener, make_service)
        .with_graceful_shutdown(async move {
            let _ = axum_shutdown_rx.recv().await;
            info!("mwe-mcp serve: shutting down axum");
        })
        .await
        .context("axum::serve")?;

    if let Some(handle) = scheduler_handle {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
            Ok(Ok(())) => info!("rem scheduler: joined cleanly"),
            Ok(Err(e)) => warn!(error = %e, "rem scheduler: task panicked on shutdown"),
            Err(_) => warn!("rem scheduler: did not exit within 5s timeout"),
        }
    }
    if let Some(handle) = light_handle {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
            Ok(Ok(())) => info!("light dream: joined cleanly"),
            Ok(Err(e)) => warn!(error = %e, "light dream: task panicked on shutdown"),
            Err(_) => warn!("light dream: did not exit within 5s timeout"),
        }
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), document_handle).await {
        Ok(Ok(())) => info!("document worker: joined cleanly"),
        Ok(Err(e)) => warn!(error = %e, "document worker: task panicked on shutdown"),
        Err(_) => warn!("document worker: did not exit within 5s timeout"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), backup_handle).await {
        Ok(Ok(())) => info!("backup scheduler: joined cleanly"),
        Ok(Err(e)) => warn!(error = %e, "backup scheduler: task panicked on shutdown"),
        Err(_) => warn!("backup scheduler: did not exit within 5s timeout"),
    }

    // A dashboard-requested restart exits with the deliberate non-zero
    // code so a `Restart=on-failure` systemd unit relaunches the
    // process (a clean exit would stay down). Unsupervised runs just
    // stop — the Backup console says so before offering the button.
    if restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
        info!(
            code = RESTART_EXIT_CODE,
            "mwe-mcp serve: restart requested from the dashboard — exiting for the supervisor"
        );
        std::process::exit(RESTART_EXIT_CODE);
    }

    Ok(())
}

/// Exit code of a dashboard-requested restart: `EX_TEMPFAIL` (75) —
/// "temporary failure, retry" — chosen so the provisioned
/// `Restart=on-failure` systemd unit relaunches the process while a
/// deliberate stop (ctrl-c, `systemctl stop`) still exits clean.
const RESTART_EXIT_CODE: i32 = 75;

/// Run `LlmBackend::health_check` on every slot the operator has wired
/// in `mwe-mcp.config.yaml > llm:`.
///
/// Refuses to continue if even one configured slot fails (no silent
/// fallbacks). Slots that are absent from the config are skipped — that
/// is a deliberate choice to disable a function, not a misconfiguration —
/// and a Claude Code login slot awaiting authentication is allowed
/// through (the operator logs in from the dashboard once the server is
/// up). The per-slot probe is the shared
/// [`mwe_core::diagnostics::probe_llm_slots`] the dashboard health page
/// also uses; unlike that read-only view this gate aggregates the
/// failures and refuses to bind when any remain.
async fn health_check_llm_slots(config: &mwe_core::config::LlmConfig) -> Result<()> {
    use mwe_core::diagnostics::{self, SlotStatus};

    let report = diagnostics::probe_llm_slots(config, |func| {
        config
            .slot(func)
            .context("llm slot vanished between probe and backend build")?
            .build_backend(func)
            .map(Arc::from)
            .map_err(anyhow::Error::new)
    })
    .await;

    for s in &report {
        match &s.status {
            SlotStatus::Reachable { backend, model } => {
                tracing::info!(slot = s.slot, %backend, %model, "llm health-check ok");
            },
            SlotStatus::Unconfigured => {
                tracing::debug!(
                    slot = s.slot,
                    "llm health-check: slot unconfigured, skipping"
                );
            },
            SlotStatus::LoginPending => {
                warn!(
                    slot = s.slot,
                    "llm health-check: Claude Code login slot not authenticated yet — boot \
                     continues; log in from the dashboard (Admin → LLM config). This slot's \
                     feature is unavailable until then."
                );
            },
            SlotStatus::Failed(detail) => {
                tracing::error!(slot = s.slot, detail, "llm health-check failed");
            },
        }
    }

    let failed = diagnostics::slots_failed(&report);
    if failed.is_empty() {
        let checked = report
            .iter()
            .filter(|s| matches!(s.status, SlotStatus::Reachable { .. }))
            .count();
        if checked == 0 {
            tracing::info!(
                "llm health-check: no reachable slots (every LLM-driven feature is off or pending)"
            );
        } else {
            tracing::info!(
                slots = checked,
                "llm health-check: all configured slots reachable"
            );
        }
        Ok(())
    } else {
        let names: Vec<&str> = failed.iter().map(|s| s.slot).collect();
        Err(anyhow!(
            "{} llm slot(s) unreachable: {}. The configured slots must be reachable — \
             fix the deploy and rerun `mwe-mcp serve`.",
            failed.len(),
            names.join(", ")
        ))
    }
}

/// Run the WAL apply driver over both stale proposal ops and stale REM
/// ops with a [`NoopInverse`] (a floor — per-kind inverses are wired
/// later).
async fn sweep_stale_wal(pool: &sqlx::SqlitePool) -> Result<()> {
    let rb_props = wal::rollback_stale_proposals(pool, DEFAULT_STALE_AFTER, &NoopInverse).await?;
    let rb_rems = wal::rollback_stale_rems(pool, DEFAULT_STALE_AFTER, &NoopInverse).await?;
    if rb_props.rolled_back + rb_rems.rolled_back > 0 {
        warn!(
            proposal_ops = rb_props.rolled_back,
            rem_ops = rb_rems.rolled_back,
            "WAL recovery: stale ops swept (NoopInverse)"
        );
    } else {
        info!("WAL recovery: clean");
    }
    Ok(())
}

/// Apply a staged dashboard recovery (restore / memory reset), if one
/// is pending: under the lockfile, before anything opens the DB or the
/// wiki tree — the only moment nothing else has a handle on the
/// workdir. A refusal boots normally with the workdir untouched; a
/// mid-apply failure is fatal (the error names the automatic safety
/// snapshot). See [`mwe_core::recovery`].
async fn apply_staged_recovery(workdir: &Path, config: &Config) -> Result<()> {
    let snapshots_dir = config.backup.snapshots_dir(workdir);
    match mwe_core::recovery::apply_pending(workdir, &snapshots_dir).await {
        Ok(None) => {},
        Ok(Some(outcome)) if outcome.ok => info!(
            action = %outcome.action,
            detail = %outcome.detail,
            "staged recovery applied at boot"
        ),
        Ok(Some(outcome)) => warn!(
            action = %outcome.action,
            detail = %outcome.detail,
            "staged recovery refused — workdir untouched"
        ),
        Err(e) => return Err(anyhow!("staged recovery: {e}")),
    }
    Ok(())
}

/// Shared startup helper used by both transports.
///
/// 1. Health-check every configured LLM slot (no silent fallbacks).
/// 2. Acquire the workdir lockfile (a static leak — held until process
///    exits; not needed once we expose stop signals).
/// 3. Apply a staged recovery, if the dashboard left one pending.
/// 4. Open + migrate `engine.db`.
/// 5. Load `MWE_TOKEN_SECRET` and prime the blacklist cache.
/// 6. Run the WAL apply driver with a [`NoopInverse`] over both stale
///    proposal ops and stale REM ops (a floor — per-kind inverses
///    are wired later).
/// 7. Open the memory-wiki tree.
/// 8. Build the shared `McpState` and the matching `DashboardState`
///    (cloned from the same handles).
async fn bootstrap_state(workdir: &Path, config: &Config) -> Result<(McpState, DashboardState)> {
    // Every configured LLM slot must be reachable before we
    // accept traffic. Refuse to bind the listener if even one slot
    // fails its health check. The check runs *before* the lockfile
    // is taken so a misconfigured deploy can be diagnosed and rerun
    // without contention with a previous instance.
    health_check_llm_slots(&config.llm).await?;

    let lock = lockfile::acquire(workdir).map_err(|e| anyhow!("lockfile: {e}"))?;
    // Lockfile must outlive the function; leak so the OS releases it at
    // process exit. A clean shutdown path that drops the guard
    // explicitly is wired later.
    Box::leak(Box::new(lock));

    apply_staged_recovery(workdir, config).await?;

    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;

    let secret = ensure_secret(workdir).context("resolving MWE_TOKEN_SECRET")?;

    sweep_stale_wal(&pool).await?;

    let blacklist = Arc::new(BlacklistCache::new());
    blacklist.refresh(&pool).await?;

    let delegations = Arc::new(DelegationCache::new());
    delegations.refresh(&pool).await?;

    let tree = WikiTree::open(workdir).context("opening wikis/ tree")?;

    // Sweep orphan write-in-progress markers before arming the
    // watcher: a crashed writer can leave a marker behind whose mtime is
    // still fresh enough to suppress its own target's first event after
    // restart.
    match sweep_stale_markers(tree.wikis_dir()) {
        Ok(0) => {},
        Ok(n) => info!(
            removed = n,
            "watcher: swept stale write-in-progress markers"
        ),
        Err(e) => warn!(error = %e, "watcher: marker sweep failed"),
    }

    // Boot-time `wiki_id` reconcile safety net: re-derive each active fact
    // row's wiki from its `source_path` (longest directory prefix over the
    // discovered wiki set — sub-wikis nest, so only the walked tree knows the
    // boundaries) and fix divergent rows with targeted UPDATEs. Idempotent;
    // a failure is non-fatal (the rows stay as they were).
    match reindex::reconcile_wiki_ids(&pool, &tree).await {
        Ok(r) if r.fixed > 0 || r.unknown > 0 => info!(
            scanned = r.scanned,
            fixed = r.fixed,
            unknown = r.unknown,
            "wiki-id reconcile: divergent rows repaired at boot"
        ),
        Ok(r) => info!(scanned = r.scanned, "wiki-id reconcile: clean"),
        Err(e) => warn!(error = %e, "wiki-id reconcile failed (non-fatal)"),
    }

    // Refresh the operator's Obsidian collector index (`wikis/index.md`) to
    // realign after any external edits/deletions while the server was down.
    // Admin convenience only — best-effort, never blocks serving.
    if let Err(e) = mwe_core::wiki::write_root_collector_index(&tree) {
        warn!(error = %e, "root collector index: bootstrap refresh failed (non-fatal)");
    }

    // The embedder backend is operator-configurable via the `embedding:`
    // section (roadmap group 18); `build_embedder` honours it, defaulting
    // to Ollama bge-m3 on localhost. The Ollama constructor does no startup
    // probe, so the dispatcher comes up regardless of whether Ollama is
    // reachable — per-call embedding requests surface the failure to the
    // caller.
    let embedder: Arc<dyn Embedder> = config
        .embedding
        .build_embedder()
        .await
        .context("building embedder")?;

    // Embedder-identity guard (roadmap 18g): if the configured embedder
    // differs from the one the store's vectors were built with, similarity
    // search is wrong until a full reindex re-embeds every fact. Surface it
    // loudly; never fatal (the operator may be mid-migration).
    match reindex::check_embedder_identity(&pool, embedder.as_ref()).await {
        Ok(reindex::EmbedderIdentity::Mismatch {
            stored_model,
            stored_dim,
            configured_model,
            configured_dim,
        }) => warn!(
            %stored_model,
            stored_dim,
            %configured_model,
            configured_dim,
            "embedder changed since the store was built — recall/similarity will be WRONG until a \
             full reindex re-embeds every fact; revert the `embedding` config or rebuild the index"
        ),
        Ok(_) => {},
        Err(e) => warn!(error = %e, "embedder-identity check failed (non-fatal)"),
    }

    // Arm the filesystem watcher + spawn the reindex consumer +
    // safety-net loop. The watcher handle is leaked so it outlives this
    // function for the program lifetime; a clean shutdown that drops it
    // explicitly is wired later. The returned sender feeds the same
    // reindex queue — `wiki_admin_push` enqueues its pages there instead
    // of embedding inline on the request path (the marker protocol hides
    // our own writes from the watcher, so without it push-written pages
    // would wait for the safety-net sweep).
    let reindex_tx = spawn_reindex_pipeline(pool.clone(), tree.clone(), embedder.clone())?;

    // One shared handle for the operator recall settings: the dashboard
    // recall-settings editor swaps it in place and both transports (MCP
    // dispatcher + dashboard chat) read it per turn — hot reload, no
    // restart caveat.
    let recall_settings = std::sync::Arc::new(std::sync::RwLock::new(config.recall.clone()));

    // Same idiom for the REM policy: the dashboard REM settings editor
    // swaps it in place; the interval scheduler (which clones this Arc
    // out of the dashboard state in `cmd_serve_http`) snapshots it at
    // each cycle start and the Dream console at each trigger.
    let rem_policy = std::sync::Arc::new(std::sync::RwLock::new(config.rem.resolved_policy()));

    let state = McpState {
        pool: pool.clone(),
        tree: tree.clone(),
        embedder: embedder.clone(),
        secret: secret.clone(),
        blacklist: blacklist.clone(),
        delegations: delegations.clone(),
        llm_config: config.llm.clone(),
        recall: recall_settings.clone(),
        workdir: workdir.to_path_buf(),
        document_policy: config.document.resolved_policy(),
        reindex_tx: Some(reindex_tx),
    };
    let dashboard_state = DashboardState::new(pool, secret, blacklist, delegations)
        .with_memory(MemoryHandles {
            tree,
            embedder,
            // Shared behind Arc<RwLock<_>> so the admin
            // LLM-config editor can swap slots in place + close the
            // restart-required gap (the MCP transport still holds its
            // own cloned copy in McpState — that side is rebuilt at
            // boot per the design note in admin-llm-config.md).
            llm_config: std::sync::Arc::new(std::sync::RwLock::new(config.llm.clone())),
            // Production path constructs the per-slot backend on every
            // request via `LlmConfig::build_backend`; only test fixtures
            // populate this for deterministic e2e runs.
            llm_overrides: mwe_dashboard::LlmBackendOverrides::default(),
            // In-memory API key override map. Empty at process
            // start; the dashboard set-API-key handler writes through
            // it so the next backend_for sees fresh keys without an
            // unsafe std::env::set_var.
            api_key_overrides: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            workdir: workdir.to_path_buf(),
        })
        .with_rem_policy(rem_policy)
        .with_recall(recall_settings)
        // Backup-schedule handle, same hot-swap idiom: the Backup
        // console swaps it in place; the backup scheduler reads it
        // fresh at each due-check.
        .with_backup_schedule(std::sync::Arc::new(std::sync::RwLock::new(Some(
            config.backup.resolved_schedule(workdir),
        ))));
    Ok((state, dashboard_state))
}

/// Stamp `charset=utf-8` onto bare `/mcp` response `Content-Type`s.
///
/// JSON is UTF-8 by definition (RFC 8259), so the parameter is redundant
/// for a correct client — but naive HTTP stacks (PowerShell 5.1, older
/// Java) decode a parameter-less body as ISO-8859-1 and mojibake every
/// non-ASCII byte. Explicit is free; a Content-Type that already carries
/// a charset (or any other type) passes through untouched.
async fn mcp_utf8_charset(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    let stamped = match resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        Some("application/json") => Some("application/json; charset=utf-8"),
        Some("text/event-stream") => Some("text/event-stream; charset=utf-8"),
        _ => None,
    };
    if let Some(ct) = stamped {
        resp.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static(ct),
        );
    }
    resp
}

/// Arm the [`WikiWatcher`] over `<workdir>/wikis/`, spawn the reindex
/// consumer that forwards every watched change to
/// [`reindex::reindex_file`], and spawn a parallel safety-net loop that
/// re-runs [`reindex::reindex_full`] every [`SAFETY_NET_INTERVAL`].
///
/// Returns the queue's second producer handle — `McpState.reindex_tx` —
/// so `wiki_admin_push` can enqueue its own written pages (the marker
/// protocol hides self-writes from the watcher). Both join handles are
/// discarded intentionally: the watcher loop terminates when the channel
/// closes (which only happens at process exit since we leak the
/// watcher), and the safety-net loop runs forever; neither needs
/// join-on-shutdown wiring yet.
fn spawn_reindex_pipeline(
    pool: sqlx::SqlitePool,
    tree: WikiTree,
    embedder: Arc<dyn Embedder>,
) -> Result<tokio::sync::mpsc::UnboundedSender<mwe_core::watcher::WatchedChange>> {
    let wikis_dir = tree.wikis_dir().to_path_buf();
    let (watcher, tx, rx) =
        WikiWatcher::start(&wikis_dir).context("starting filesystem watcher")?;
    // Leak the watcher: drop tears down the underlying `notify` thread.
    Box::leak(Box::new(watcher));

    let tree_arc = Arc::new(tree);
    // Explicit `drop` over `let _ =` so clippy's let_underscore_future
    // does not warn — `JoinHandle` is `Future`-shaped but we are
    // intentionally fire-and-forget here (the loops own their lifetimes
    // via the leaked watcher and the interval ticker).
    drop(reindex::spawn_watcher_loop(
        pool.clone(),
        tree_arc.clone(),
        embedder.clone(),
        rx,
    ));
    drop(reindex::spawn_safety_net_loop(
        pool,
        tree_arc,
        embedder,
        SAFETY_NET_INTERVAL,
    ));
    info!(
        wikis_dir = %wikis_dir.display(),
        safety_net_seconds = SAFETY_NET_INTERVAL.as_secs(),
        "watcher: armed and reindex consumer + safety net spawned"
    );
    Ok(tx)
}

/// Mint a JWT and print the encoded token to stdout. The
/// module path is identical whether the token will be used by an MCP
/// client or by the dashboard — only the TTL differs.
#[allow(clippy::too_many_arguments)]
async fn cmd_token_issue(
    workdir: &Path,
    sender: &str,
    device: &str,
    rate_limit_id: &str,
    ttl_profile: &str,
    is_admin: bool,
    consumer_id: Option<String>,
    consumer_class: ConsumerClass,
) -> Result<()> {
    // Open the DB read-only so we can sanity-check that `sender` is a
    // known user. Catches typos before they ship as tokens.
    let _lock = lockfile::acquire(workdir).map_err(|e| anyhow!("lockfile: {e}"))?;
    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;

    let known: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollment_users WHERE user_id = ?")
        .bind(sender)
        .fetch_one(&pool)
        .await?;
    if known == 0 {
        bail!(
            "sender {sender:?} is not in enrollment_users; create the user from the dashboard \
             before issuing tokens"
        );
    }

    // Diagonal identity model: the connection pattern is
    // a function of `consumer_class`, not a free per-deployment choice. The
    // policy is shared with the dashboard token form via
    // `enrollment::validate_token_identity` so the two never drift.
    if let Err(msg) = mwe_core::enrollment::validate_token_identity(
        &pool,
        sender,
        consumer_class,
        consumer_id.is_some(),
    )
    .await
    {
        bail!("{msg}");
    }

    let secret = load_secret_from_env()?;
    let ttl = match ttl_profile {
        "internal" => DEFAULT_INTERNAL_TTL,
        "exposed" => DEFAULT_EXPOSED_TTL,
        other => bail!("unknown ttl profile: {other}"),
    };

    let mut claims = TokenClaims::new(sender, device, rate_limit_id, ttl);
    claims.is_admin = is_admin;
    claims.consumer_id = consumer_id;
    claims.consumer_class = consumer_class;
    let token = jwt::issue(&secret, &claims).context("signing token")?;

    println!("sender_id      : {}", claims.sender_id);
    println!("device_label   : {}", claims.device_label);
    println!("rate_limit_id  : {}", claims.rate_limit_id);
    println!("jti            : {}", claims.jti);
    println!("iat            : {} ({})", claims.iat, rfc3339(claims.iat));
    println!("exp            : {} ({})", claims.exp, rfc3339(claims.exp));
    println!("isAdmin        : {}", claims.is_admin);
    if let Some(c) = &claims.consumer_id {
        println!("consumer_id    : {c}");
    }
    // Only print the class line when smart; standard is the silent
    // default both on the wire and in CLI output.
    if claims.consumer_class.is_smart() {
        println!("consumer_class : smart");
    }
    println!();
    println!("token: {token}");
    Ok(())
}

/// Insert a revoke row into `token_blacklist`.
async fn cmd_token_revoke(
    workdir: &Path,
    jti: &str,
    reason: &str,
    revoked_by: Option<String>,
    original_exp: Option<i64>,
) -> Result<()> {
    let _lock = lockfile::acquire(workdir).map_err(|e| anyhow!("lockfile: {e}"))?;
    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;

    let actor = revoked_by
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "cli".to_owned());
    let exp = original_exp.unwrap_or_else(|| {
        chrono::Utc::now().timestamp() + i64::try_from(DEFAULT_INTERNAL_TTL.as_secs()).unwrap_or(0)
    });

    jwt::revoke(&pool, jti, reason, &actor, exp)
        .await
        .context("inserting token blacklist entry")?;
    println!("revoked: jti={jti} reason={reason:?} revoked_by={actor}");
    Ok(())
}

/// Row shape for `cmd_token_list` — five `Option<String>`/`String`
/// columns of `token_blacklist`. Hoisted to module level because clippy
/// pedantic rejects `type` aliases inside function bodies.
type TokenBlacklistRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// List the contents of `token_blacklist`. Per the discussion
/// captured in the maintainer notes: "active tokens" are not enumerable
/// server-side: we list the revocations we persist.
async fn cmd_token_list(workdir: &Path) -> Result<()> {
    let _lock = lockfile::acquire(workdir).map_err(|e| anyhow!("lockfile: {e}"))?;
    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;

    let rows: Vec<TokenBlacklistRow> = sqlx::query_as(
        "SELECT jti, revoked_at, expires_at, reason, revoked_by
           FROM token_blacklist
          ORDER BY revoked_at DESC",
    )
    .fetch_all(&pool)
    .await?;

    if rows.is_empty() {
        println!("token_blacklist: empty");
        return Ok(());
    }

    println!(
        "{:<38} {:<26} {:<26} {:<20} reason",
        "jti", "revoked_at", "expires_at", "revoked_by",
    );
    for (jti, revoked_at, expires_at, reason, revoked_by) in rows {
        println!(
            "{:<38} {:<26} {:<26} {:<20} {}",
            jti,
            revoked_at,
            expires_at.unwrap_or_else(|| "-".to_owned()),
            revoked_by.unwrap_or_else(|| "-".to_owned()),
            reason.unwrap_or_else(|| "-".to_owned()),
        );
    }
    Ok(())
}

/// Break-glass password recovery — see [`Command::AdminReset`] for
/// the model. Implementation: validate that the user exists, mint a
/// fresh `user_invitations` row with a `UUIDv7` id, print the accept
/// URL placeholder for the admin to share out of band.
async fn cmd_admin_reset(
    workdir: &Path,
    user_id: &str,
    ttl_hours: u32,
    invited_by: Option<String>,
    clear_2fa: bool,
) -> Result<()> {
    let _lock = lockfile::acquire(workdir).map_err(|e| anyhow!("lockfile: {e}"))?;
    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;

    let known: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollment_users WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(&pool)
        .await?;
    if known == 0 {
        bail!(
            "user {user_id:?} is not in enrollment_users; create the user from the dashboard \
             before resetting credentials"
        );
    }

    if clear_2fa {
        // Break-glass for a lost authenticator: drop the enrollment + its
        // recovery codes so the user signs in with just the new password
        // and re-enrols. The ON DELETE CASCADE covers the codes, but we
        // delete both explicitly so the intent is on the record.
        sqlx::query("DELETE FROM user_2fa_recovery_codes WHERE user_id = ?")
            .bind(user_id)
            .execute(&pool)
            .await
            .context("clearing 2fa recovery codes")?;
        sqlx::query("DELETE FROM user_2fa WHERE user_id = ?")
            .bind(user_id)
            .execute(&pool)
            .await
            .context("clearing 2fa enrollment")?;
        println!("Cleared two-factor (TOTP) enrollment for {user_id}.");
        println!();
    }

    let actor = invited_by
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "cli".to_owned());

    let invitation_id =
        uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new())).to_string();
    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::hours(i64::from(ttl_hours));

    sqlx::query(
        "INSERT INTO user_invitations
            (invitation_id, user_id, created_at, expires_at, consumed_at, invited_by)
         VALUES (?, ?, ?, ?, NULL, ?)",
    )
    .bind(&invitation_id)
    .bind(user_id)
    .bind(now.to_rfc3339())
    .bind(expires_at.to_rfc3339())
    .bind(&actor)
    .execute(&pool)
    .await
    .context("inserting invitation row")?;

    println!("invitation_id : {invitation_id}");
    println!("user_id       : {user_id}");
    println!("invited_by    : {actor}");
    println!("expires_at    : {}", expires_at.to_rfc3339());
    println!();
    println!("Share this URL out of band with the user:");
    println!();
    println!("    /dashboard/accept-invite/{invitation_id}");
    println!();
    println!("Once they consume the invitation their existing user_credentials row");
    println!("(if any) is overwritten with the new password they choose. The admin");
    println!("never sees the password.");
    Ok(())
}

/// Offline health check — fails non-zero on a broken invariant. The
/// checks mirror what `serve` does at startup so an operator can
/// reproduce a boot failure with `mwe-mcp doctor`.
///
/// `doctor` is the **boot-failure triage** tool: it acquires the workdir
/// lockfile (so it deliberately fails when a `serve` is already running),
/// reads `MWE_TOKEN_SECRET` from the env, and runs a JWT self-test —
/// checks an in-server endpoint cannot serve. The lockfile-free subset
/// (DB / WAL / blacklist / perms / LLM-slot reachability) is the shared
/// [`mwe_core::diagnostics`] collector the dashboard health page surfaces
/// against the *running* server (roadmap group 19).
async fn cmd_doctor(workdir: &Path) -> Result<()> {
    println!("workdir       : {}", workdir.display());
    println!(
        "lockfile path : {}",
        lockfile::lockfile_path(workdir).display()
    );

    if std::env::var(SECRET_ENV).is_ok() {
        let secret = load_secret_from_env()?;
        println!(
            "token secret  : present ({SECRET_ENV}, {} bytes)",
            secret.len()
        );
    } else {
        println!("token secret  : MISSING ({SECRET_ENV})");
    }

    // Lockfile + DB. We deliberately acquire the lockfile here too —
    // doctor is meant to fail if a serve is currently running; the
    // operator can re-run after stopping the server (or use the dashboard
    // health page, which needs no lockfile).
    let _lock = lockfile::acquire(workdir).map_err(|e| anyhow!("lockfile: {e}"))?;
    println!("lockfile      : acquired");

    let pool = db::open_or_init(workdir)
        .await
        .context("opening engine.db")?;

    let d = diagnostics::collect_db(&pool, workdir)
        .await
        .context("collecting diagnostics")?;
    println!(
        "engine.db     : open, {} app tables, {} migrations applied",
        d.app_tables, d.applied_migrations
    );
    println!(
        "WAL recovery  : {} stale proposal ops, {} stale REM ops",
        d.stale_proposal_ops, d.stale_rem_ops
    );
    println!("token_blacklist: {} entries", d.token_blacklist_entries);

    // Workdir reachability: the wiki bytes are cleartext on disk, so the
    // per-reader ACL only holds if non-server principals cannot read them.
    if d.perm_findings.is_empty() {
        println!("workdir perms : owner-only (no group/world access)");
    } else {
        println!(
            "workdir perms : {} path(s) reachable by other principals (per-reader ACL bypassable):",
            d.perm_findings.len()
        );
        for f in &d.perm_findings {
            println!(
                "                [{}] {} {}",
                f.severity.tag(),
                f.mode_string(),
                f.path.display()
            );
        }
        println!(
            "                fix: {}  (or run the consumer on a separate host/user — \
             see INTEGRATING.md \"Deployment security\")",
            workdir_security::remediation(workdir)
        );
    }

    // Touch the JWT module so the secret is exercised end-to-end.
    if let Ok(secret) = load_secret_from_env() {
        let test_claims =
            TokenClaims::new("doctor", "self-test", "default", Duration::from_secs(60));
        let token = jwt::issue(&secret, &test_claims).context("signing test token")?;
        let _ = jwt::verify_offline(&secret, &token).context("verifying test token")?;
        let _cache = BlacklistCache::new();
        println!("jwt self-test : ok");
    }

    // Same LLM-slot health check `mwe-mcp serve` runs at boot, exposed
    // on-demand so an operator can diagnose / re-verify post-config-change
    // without restarting the server.
    let config = Config::load(workdir).context("loading mwe-mcp.config.yaml")?;
    match health_check_llm_slots(&config.llm).await {
        Ok(()) => println!("llm slots     : every configured slot reachable"),
        Err(e) => {
            println!("llm slots     : FAIL — {e:#}");
            return Err(e);
        },
    }

    println!("doctor        : ok");
    Ok(())
}

fn load_secret_from_env() -> Result<TokenSecret> {
    let raw = std::env::var(SECRET_ENV).map_err(|_| anyhow!("{SECRET_ENV} not set in env"))?;
    let bytes = if raw.len() == MIN_SECRET_BYTES * 2 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        hex::decode(&raw).context("decoding hex secret")?
    } else {
        raw.into_bytes()
    };
    TokenSecret::new(bytes).map_err(|e| anyhow!("{e}"))
}

/// Resolve the JWT signing secret for `serve`, self-bootstrapping it on
/// first boot (roadmap [group 19](../../../docs/development/build-run.md)).
///
/// `serve` no longer requires a prior `mwe-mcp init`: on an empty workdir
/// it generates a fresh `MWE_TOKEN_SECRET`, persists it to
/// `<workdir>/mwe-mcp.env` (mode `0o600` on unix), and uses it — so the
/// daemon comes up self-sufficient and the admin only completes identity
/// and LLM config from the dashboard wizard. Subsequent boots find the
/// secret in the loaded env and reuse it verbatim.
///
/// Resolution order:
/// 1. `MWE_TOKEN_SECRET` already in the process env (set by the parent
///    shell or loaded from `mwe-mcp.env` at startup) — used as-is.
/// 2. Otherwise a fresh secret is generated and written to
///    `mwe-mcp.env` when that file is absent.
/// 3. If `mwe-mcp.env` exists but defines no secret, that is treated as
///    operator intent we must not silently overwrite — surfaced as an
///    error so they add `MWE_TOKEN_SECRET` or remove the file.
fn ensure_secret(workdir: &Path) -> Result<TokenSecret> {
    load_secret_from_env().map_or_else(|_| generate_and_persist_secret(workdir), Ok)
}

/// Generate a fresh `MWE_TOKEN_SECRET` and persist it to
/// `<workdir>/mwe-mcp.env`, returning the secret for immediate use.
///
/// Split out of [`ensure_secret`] so the persistence behaviour is
/// testable without mutating the process-global environment. Refuses to
/// clobber an existing `mwe-mcp.env` that simply lacks the variable.
///
/// # Errors
///
/// Propagates I/O failures from writing `mwe-mcp.env`, and errors when
/// the file already exists but defines no secret.
fn generate_and_persist_secret(workdir: &Path) -> Result<TokenSecret> {
    let candidate = TokenSecret::generate();
    match env_loader::write_env_file_if_needed(workdir, &candidate.export_hex(), false)
        .context("persisting generated MWE_TOKEN_SECRET")?
    {
        WriteOutcome::Wrote { path, chmod_0600 } => {
            info!(
                path = %path.display(),
                chmod_0600,
                "serve: generated and persisted a fresh MWE_TOKEN_SECRET (first boot)"
            );
            Ok(candidate)
        },
        WriteOutcome::Preserved { path } => Err(anyhow!(
            "{} exists but defines no {SECRET_ENV}; add the variable or delete the file so \
             `serve` can generate one",
            path.display()
        )),
    }
}

fn rfc3339(unix_secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix_secs, 0)
        .map_or_else(|| "<invalid>".to_owned(), |dt| dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `/mcp` charset middleware stamps `charset=utf-8` onto bare
    /// `application/json` responses and leaves other types untouched —
    /// PS 5.1-class clients decode a parameter-less body as ISO-8859-1.
    #[tokio::test]
    async fn mcp_utf8_charset_stamps_bare_json_and_leaves_the_rest() {
        use tower::ServiceExt as _;
        let app = Router::new()
            .route(
                "/json",
                axum::routing::get(|| async {
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        "{}",
                    )
                }),
            )
            .route(
                "/plain",
                axum::routing::get(|| async {
                    ([(axum::http::header::CONTENT_TYPE, "text/plain")], "x")
                }),
            )
            .layer(axum::middleware::from_fn(mcp_utf8_charset));

        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::get("/json")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.headers()[axum::http::header::CONTENT_TYPE],
            "application/json; charset=utf-8"
        );

        let resp = app
            .oneshot(
                axum::http::Request::get("/plain")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.headers()[axum::http::header::CONTENT_TYPE],
            "text/plain"
        );
    }

    /// On an empty workdir the secret is generated, returned for
    /// immediate use, and persisted to `mwe-mcp.env` so the next boot
    /// finds it in the loaded env.
    #[test]
    fn generate_and_persist_secret_writes_and_returns_matching_secret() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = generate_and_persist_secret(dir.path()).expect("generate secret");

        let body = std::fs::read_to_string(dir.path().join("mwe-mcp.env")).expect("read env file");
        assert!(
            body.contains(&format!("MWE_TOKEN_SECRET={}", secret.export_hex())),
            "persisted env file must carry the returned secret verbatim, got:\n{body}"
        );
    }

    /// An existing `mwe-mcp.env` that defines no secret is operator
    /// intent we must not silently overwrite — the helper errors instead
    /// of clobbering the file.
    #[test]
    fn generate_and_persist_secret_refuses_to_clobber_secretless_env_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("mwe-mcp.env"),
            "# operator notes, no secret here\n",
        )
        .expect("seed env file");

        let err = generate_and_persist_secret(dir.path())
            .expect_err("must refuse to clobber a secretless env file");
        assert!(
            err.to_string().contains("defines no MWE_TOKEN_SECRET"),
            "error must name the missing variable, got: {err}"
        );
    }

    /// The provisioned systemd unit must run as the dedicated account, on the
    /// caller's bind/port, with boot + auto-restart — and must NOT carry the
    /// bypass flag (it passes the gate as a nologin account on its own).
    #[test]
    fn service_unit_renders_dedicated_runtime() {
        let unit = service_unit(
            PROD_BIN,
            DEDICATED_WORKDIR,
            IpAddr::from([127, 0, 0, 1]),
            8742,
            DEDICATED_USER,
            false,
        );
        assert!(unit.contains(&format!("User={DEDICATED_USER}")), "{unit}");
        assert!(
            unit.contains(&format!("WorkingDirectory={DEDICATED_WORKDIR}")),
            "{unit}"
        );
        assert!(
            unit.contains(&format!(
                "ExecStart={PROD_BIN} serve --workdir {DEDICATED_WORKDIR} --bind 127.0.0.1 --port 8742"
            )),
            "{unit}"
        );
        assert!(unit.contains("Restart=on-failure"), "{unit}");
        assert!(unit.contains("WantedBy=multi-user.target"), "{unit}");
        // ProtectSystem=strict makes everything read-only; the workdir must be
        // re-opened for writes or the service can't persist memory.
        assert!(unit.contains("ProtectSystem=strict"), "{unit}");
        assert!(
            unit.contains(&format!("ReadWritePaths={DEDICATED_WORKDIR}")),
            "{unit}"
        );
        // ...and the bge-m3 weight cache must land inside that writable workdir,
        // not the default $HOME/.cache the sandbox would deny on first run.
        assert!(
            unit.contains(&format!(
                "Environment=XDG_CACHE_HOME={DEDICATED_WORKDIR}/.cache"
            )),
            "{unit}"
        );
        assert!(
            !unit.contains("--bypassdedicateduser"),
            "the service runs as the dedicated user and must not bypass the gate:\n{unit}"
        );
    }

    /// The bypass service (single-purpose host, no co-located consumer) runs as
    /// the operator's own login account and **must** carry `--bypassdedicateduser`
    /// in `ExecStart`, or it would refuse to boot under that login account.
    #[test]
    fn service_unit_bypass_runs_as_login_user_with_flag() {
        let unit = service_unit(
            "/usr/local/bin/mwe-mcp",
            "/home/ubuntu/work",
            IpAddr::from([0, 0, 0, 0]),
            8742,
            "ubuntu",
            true,
        );
        assert!(unit.contains("User=ubuntu"), "{unit}");
        assert!(
            unit.contains(
                "ExecStart=/usr/local/bin/mwe-mcp serve --workdir /home/ubuntu/work \
                 --bind 0.0.0.0 --port 8742 --bypassdedicateduser"
            ),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=XDG_CACHE_HOME=/home/ubuntu/work/.cache"),
            "{unit}"
        );
    }

    /// Explicit `--bind`/`--port` are honoured verbatim, and a flagless call in
    /// a non-interactive context (as under the test harness, systemd, CI) falls
    /// back to the loopback defaults rather than blocking on a prompt.
    #[test]
    fn resolve_exposure_honours_flags_and_defaults_non_interactively() {
        let explicit = IpAddr::from([0, 0, 0, 0]);
        assert_eq!(
            resolve_exposure(Some(explicit), Some(9000)),
            (explicit, 9000)
        );
        // No terminal in the test harness → no prompt, just the defaults / the
        // one flag that was set.
        assert_eq!(resolve_exposure(None, None), (DEFAULT_BIND, DEFAULT_PORT));
        assert_eq!(
            resolve_exposure(Some(explicit), None),
            (explicit, DEFAULT_PORT)
        );
        assert_eq!(resolve_exposure(None, Some(9000)), (DEFAULT_BIND, 9000));
    }

    /// A non-default bind/port chosen on the command line must propagate into
    /// the unit's `ExecStart` so the service serves where the operator asked.
    #[test]
    fn service_unit_propagates_custom_bind_and_port() {
        let unit = service_unit(
            PROD_BIN,
            DEDICATED_WORKDIR,
            IpAddr::from([0, 0, 0, 0]),
            9000,
            DEDICATED_USER,
            false,
        );
        assert!(
            unit.contains("--bind 0.0.0.0 --port 9000"),
            "custom bind/port must reach ExecStart:\n{unit}"
        );
    }
}
