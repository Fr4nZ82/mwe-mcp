// SPDX-License-Identifier: AGPL-3.0-or-later
//! First-login profile wizard — visual front-end to `wiki_ingest_message`.
//!
//! Per the setup & identity model,
//! every freshly-created user has a working account (email + password)
//! and an empty identity wiki. The very first time they sign in
//! (`user_credentials.profile_initialized = 0`), the auth flow lands
//! them on `/dashboard/welcome` — this module — which collects optional
//! profile data and ships it to [`mwe_core::ingest::wiki_ingest_message`].
//!
//! **Three steps → three destinations** (see the
//! [memory model](../../../../docs/concepts/memory-model.md)). The form is one page with three
//! client-side steps that map to the three universal ingest
//! destinations:
//!
//! 1. **Identity + always-on → `index.md`.** The identity card plus a
//!    health/safety slot. The ingest LLM marks these `salience: high`
//!    and the engine routes the always-on core to the owner's
//!    `index.md` base context.
//! 2. **Governance rules → `rules.md`.** Privacy / sharing / do-not-store
//!    presets. The ingest LLM marks each `engine_rule: true` and the
//!    engine appends it to the sender's `rules.md` policy page
//!    — never a row in `fact_index`.
//! 3. **The rest → normal pipeline.** Low-weight preferences the LLM
//!    files wherever it sees fit.
//!
//! The routing itself is the ingest prompt's job (universal).
//! The wizard only organises the collection and adds reinforcing section
//! markers to the composed message — it is *just the collection UI*. The
//! mechanism is unchanged from the old single-step wizard: one composed
//! first-person Italian message (e.g. `favorite_color=rosso` becomes
//! "il mio colore preferito è il rosso"), one ingest call through the
//! chat chokepoint (`chat::process_submission`). The LLM `ingest`
//! slot classifies the message and emits N `wiki_capture` / engine-rule
//! routings so each piece lands at its destination, indistinguishable
//! from a chat turn.
//!
//! **No fallback.** If `llm.ingest` is not configured in
//! `mwe-mcp.config.yaml`, the `Save` action returns 422 with an
//! explicit error. The operator must wire the slot before the wizard
//! is usable. `Skip` always works — that's the only escape valve for a
//! deploy that has not configured `ingest` yet.

use std::fmt::Write as _;

use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use maud::{Markup, PreEscaped, html};
use mwe_core::config::LlmFunction;
use serde::Deserialize;

use crate::auth::SessionUser;
use crate::error::Result;
use crate::routes::chat::{self, ChatTurn};
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Sub-router for `/welcome`. Mounted inside the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/welcome", get(form).post(submit))
}

/// Fields the wizard collects. All optional except for the implicit
/// `email` which is read server-side from `user_credentials`.
///
/// The fields are grouped by the wizard's three steps, which map to the
/// three universal ingest destinations: step 1 → the user's
/// `index.md` always-on base context (identity + health/safety, marked
/// `salience: high` by the ingest LLM), step 2 → the user's `rules.md`
/// engine-policy page (privacy / do-not-store directives, routed by the
/// `engine_rule` flag the ingest LLM sets), step 3 → the normal pipeline
/// (low-weight preferences the LLM files wherever it sees fit). The
/// routing itself lives in the ingest prompt (universal); the
/// wizard only organises the collection and adds reinforcing section
/// markers to the composed message.
#[derive(Debug, Default, Deserialize)]
pub struct ProfileSubmission {
    // ── Step 1 → index.md (identity + always-on) ──
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub presentati: String,
    #[serde(default)]
    pub birthday: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub timezone: String,
    #[serde(default)]
    pub pronouns: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub occupation: String,
    /// Health / safety / hard standing constraints an assistant must
    /// know in *every* interaction (allergies, intolerances, chronic
    /// conditions, "only ever write to me in Italian"). Always-on →
    /// `index.md`, and — like the rest of the primer — framed **public**:
    /// the primer is the public-profile channel, so whatever the user types
    /// here is public. Private health goes in later via normal chat instead.
    #[serde(default)]
    pub health_safety: String,

    // ── Step 2 → rules.md (engine-governance policy) ──
    /// Q1 sharing default. One of `""` (unanswered), `"private"`,
    /// `"group"`, `"always_private"`.
    #[serde(default)]
    pub sharing_default: String,
    /// Q2 — who / which group to NEVER share with.
    #[serde(default)]
    pub sharing_exclusions: String,
    /// Q3 — topics to keep always private (health, money, work, …).
    #[serde(default)]
    pub private_topics: String,
    /// Q5 — things to NEVER store at all (do-not-store).
    #[serde(default)]
    pub do_not_store: String,

    // ── Step 3 → normal pipeline (low-weight preferences) ──
    #[serde(default)]
    pub favorite_color: String,
    #[serde(default)]
    pub hobbies: String,
    #[serde(default)]
    pub food_preferences: String,

    /// Hidden-action toggle. `"save"` means run the ingest pipeline;
    /// `"skip"` means flip the flag without capturing.
    #[serde(default)]
    pub action: String,
}

async fn form(State(state): State<DashboardState>, user: SessionUser) -> Result<Response> {
    if user_already_initialized(&state, &user.sender_id).await? {
        return Ok(Redirect::to("/dashboard/home").into_response());
    }
    let email = fetch_email_for(&state, &user.sender_id).await?;
    let ingest_available = state
        .memory
        .as_ref()
        .and_then(|m| m.defaults_for(LlmFunction::Ingest))
        .is_some();
    Ok(render_form(&user, email.as_deref(), ingest_available).into_response())
}

async fn submit(
    State(state): State<DashboardState>,
    user: SessionUser,
    axum::Form(form): axum::Form<ProfileSubmission>,
) -> Result<Response> {
    if user_already_initialized(&state, &user.sender_id).await? {
        return Ok(Redirect::to("/dashboard/home").into_response());
    }

    let want_skip = form.action.trim().eq_ignore_ascii_case("skip");

    // The welcome wizard composes a primer message and pushes it
    // through the same chat entry point an interactive user would use.
    // No more direct call to `wiki_ingest_message` here — the chat
    // module owns that path and we route through it so the resulting
    // turn lands in the user's chat history exactly as if they had
    // typed it themselves.
    let primer_turn = if want_skip {
        None
    } else {
        let email = fetch_email_for(&state, &user.sender_id).await?;
        let message = compose_ingest_message(email.as_deref(), &form);
        if message.is_empty() {
            None
        } else {
            // `llm.ingest` MUST be configured. `process_submission`
            // surfaces the missing-slot case as `DashboardError::Internal`.
            Some(chat::process_submission(&state, &user, &message).await?)
        }
    };

    // Mark the wizard done so subsequent logins go straight to home.
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1 WHERE user_id = ?")
        .bind(&user.sender_id)
        .execute(&state.pool)
        .await?;

    tracing::info!(
        user = %user.sender_id,
        skipped = want_skip,
        has_primer = primer_turn.is_some(),
        "profile wizard completed"
    );

    // Skip → straight to home. Save → land on the chat-introductory
    // page with the primer turn injected into the local history (the
    // right-side panel hydrates from localStorage on every page, so the
    // turn shows up as soon as chat.js runs).
    Ok(primer_turn.map_or_else(
        || Redirect::to("/dashboard/home").into_response(),
        |turn| Html(render_welcome_landing(&user, &turn)).into_response(),
    ))
}

/// Render the post-wizard landing page. The visible body is a brief
/// "welcome, your first message is in the panel on the right" note;
/// the heavy lifting is a small inline `<script>` that publishes the
/// primer turn to a global the chat panel's `chat.js` picks up at
/// hydration time. The script is the only piece of glue the welcome
/// flow needs; everything else (storage, rendering, scrolling) is the
/// chat panel's job.
fn render_welcome_landing(user: &SessionUser, turn: &ChatTurn) -> String {
    let response_html = chat::response_panel(&turn.response).into_string();
    let payload = serde_json::json!({
        "user_text": turn.user_text,
        "response_html": response_html,
        "ts": chrono::Utc::now().timestamp_millis(),
    });
    let payload_js = serde_json::to_string(&payload).unwrap_or_else(|_| "null".into());

    let body = html! {
        h2 { "All set." }
        p {
            "Your first message has already been processed — you'll find it in the chat panel "
            "on the right, along with what the system understood."
        }
        p.muted {
            "From now on the chat panel is the starting point for everything: write what you want to "
            "save, ask for what you want to recall. The history stays in your browser; none of it is "
            "replayed to the model — autocapture saves directly, recall finds things when needed."
        }
        p {
            a href="/dashboard/home" { "Go to the home" }
            " · "
            a href="/dashboard/wiki" { "Browse your memory" }
        }
        // Publish the primer turn so chat.js can splice it into the
        // local history on its first run. The chat panel is rendered
        // by the layout shell after this body, and chat.js loads with
        // `defer`, so the global is in place by the time the script
        // queries it.
        script {
            (PreEscaped(format!(
                "window.__mweChatPrimer = {payload_js};"
            )))
        }
    };
    layout::authenticated_page("Welcome", user, &body)
}

/// Has the user already completed (or skipped) the wizard?
pub async fn user_already_initialized(state: &DashboardState, sender_id: &str) -> Result<bool> {
    // The credential row may legitimately be absent for non-login
    // identities (consumer-only accounts). Treat absence as "already
    // initialized" so the auth flow never sticks them in the wizard.
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT profile_initialized FROM user_credentials WHERE user_id = ?")
            .bind(sender_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(match row {
        Some((flag,)) => flag != 0,
        None => true,
    })
}

async fn fetch_email_for(state: &DashboardState, sender_id: &str) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT email FROM enrollment_users WHERE user_id = ?")
            .bind(sender_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(row.and_then(|(e,)| e))
}

/// Plain-Italian consent line prepended to every non-empty primer.
///
/// The wizard collects profile data the user has explicitly chosen to
/// expose; the visible form header tells them upfront that what they
/// type lands public. We mirror that consent into the message in
/// natural-language italian — no field names, no quoted JSON keys,
/// just a sentence the user would plausibly type into the chat
/// themselves ("these are public facts about me, anyone can see them").
///
/// Rationale. The earlier draft of this primer mentioned
/// `owner_id: "global"` verbatim inside backticks, hoping the LLM
/// would propagate the keyword to its JSON plan. In practice Qwen 3.5
/// 9B Q8 ALSO copied "global" into `target_wiki_id`, hallucinating a
/// wiki by that name and crashing the capture pipeline. The fix
/// follows the architectural principle in [`AGENT_INSTRUCTIONS.md`]:
/// consumers speak natural language only; the ingest LLM and its
/// system prompt translate that into the JSON plan. "Public" is the
/// VISIBILITY axis: the system prompt maps this plain-italian "public"
/// signal to `allow_ids: ["global"]` while `owner_id` stays the subject
/// (the user filling the primer) — the fact is *about me* and *visible
/// to all*, not *owned by everyone*. A plain-italian "public" signal,
/// next to the "prefer the sender's own wiki" rule, is enough.
///
/// [`AGENT_INSTRUCTIONS.md`]: ../../../AGENT_INSTRUCTIONS.md
const PUBLIC_PROFILE_PRIMER_PREFIX: &str = "Tutto quello che scrivo qui sotto — dati biografici, \
contatti e anche le informazioni di salute o sicurezza — è informazione PUBBLICA del mio profilo: \
può essere vista da chiunque, non è riservata a me, e va trattata come pubblica senza eccezioni.";

/// Tiny vanilla script attached to the welcome form. While the POST is
/// in flight (~15 s for Ollama on the workhorse model) we keep the
/// browser navigating normally — `submit` is *not* preventDefault'd —
/// but we swap the clicked button's content to a `.spinner` + the
/// per-button `data-pending-label` ("Saving…" / "One moment…") and
/// disable both submits so the user cannot fire a second navigation
/// over the first. The disabled state is implicit once the response
/// page replaces this one; the swap exists purely for the seconds
/// between click and navigation.
const WELCOME_SUBMIT_SPINNER_JS: &str = "(function(){\
var f=document.getElementById('welcome-form');if(!f)return;\
f.addEventListener('submit',function(ev){\
var clicked=ev.submitter||f.querySelector('button[type=\"submit\"]');\
var btns=f.querySelectorAll('button[type=\"submit\"]');\
for(var i=0;i<btns.length;i++){btns[i].disabled=true;}\
if(clicked){\
var label=clicked.getAttribute('data-pending-label')||clicked.textContent;\
clicked.innerHTML='';\
var s=document.createElement('span');s.className='spinner';\
clicked.appendChild(s);\
clicked.appendChild(document.createTextNode(' '+label));\
}\
});\
})();";

/// Vanilla three-step stepper for the welcome form. Shows one
/// `[data-step]` fieldset at a time and wires the `[data-next]` /
/// `[data-back]` buttons (both `type=button`, so they never submit). The
/// form is a single POST — the steps are pure UI organisation, all three
/// fieldsets submit together regardless of which one is visible (hidden ≠
/// disabled). Degrades gracefully: with JS off, nothing is hidden and the
/// form is one long page the user can fill top-to-bottom and submit from
/// the final "Save and go" button.
const WELCOME_STEPPER_JS: &str = "(function(){\
var f=document.getElementById('welcome-form');if(!f)return;\
var steps=f.querySelectorAll('[data-step]');\
var total=steps.length;if(!total)return;\
var cur=0;\
var ind=document.getElementById('welcome-step-indicator');\
function show(i){\
cur=Math.max(0,Math.min(total-1,i));\
for(var j=0;j<total;j++){steps[j].hidden=(j!==cur);}\
if(ind){ind.textContent='Step '+(cur+1)+' of '+total;}\
}\
f.addEventListener('click',function(ev){\
var t=ev.target;\
if(!t||t.tagName!=='BUTTON')return;\
if(t.hasAttribute('data-next')){ev.preventDefault();show(cur+1);}\
else if(t.hasAttribute('data-back')){ev.preventDefault();show(cur-1);}\
});\
show(0);\
})();";

/// Step-2 section marker. Tells the ingest LLM the lines that follow are
/// the user's GOVERNANCE rules (privacy / do-not-store), not facts — so
/// it sets `engine_rule: true` and the engine appends them to
/// the sender's `rules.md` instead of filing rows in
/// `fact_index`. Reinforcement, not routing: the universal routing
/// already lives in the ingest prompt; the marker just nudges the
/// classification.
const ENGINE_RULES_SECTION_MARKER: &str = "Le indicazioni che seguono NON sono fatti su di me, \
ma le mie REGOLE su come gestire la mia memoria — privacy, condivisione, e cosa non salvare:";

/// Step-3 section marker. Low-weight personal preferences with no special
/// routing: the ingest LLM files them on whatever page it sees fit
/// (`salience` stays normal/low → never the always-on `index.md`).
const PREFERENCES_SECTION_MARKER: &str =
    "Infine, qualche preferenza e interesse personale, niente di importante:";

/// Compose the whole ingest message from the three wizard steps. Each
/// step contributes one section, in order, joined by a blank line; empty
/// sections are dropped. One message, one ingest call — the mechanism the
/// chat panel and the old single-step wizard already use (it stays
/// invariant). The per-extraction routing is the ingest prompt's job
/// (universal); the section markers only reinforce it.
///
/// Returns the empty string when no field is set at all so the caller
/// can short-circuit.
fn compose_ingest_message(email: Option<&str>, form: &ProfileSubmission) -> String {
    [
        compose_identity_section(email, form),
        compose_engine_rules_section(form),
        compose_preferences_section(form),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n")
}

/// Step 1 → `index.md`. The identity card (email / name / contacts / …),
/// the free-form `presentati`, **and** the always-on `health_safety` line
/// are all first-person prose under [`PUBLIC_PROFILE_PRIMER_PREFIX`], the
/// public-consent line the form promises. The primer is the public-profile
/// channel: everything written here is framed public (health included, the
/// ingest LLM still marks it `salience: high`); a user who wants something
/// private leaves the field blank and adds it later through normal chat.
fn compose_identity_section(email: Option<&str>, form: &ProfileSubmission) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if let Some(e) = email {
        let e = e.trim();
        if !e.is_empty() {
            clauses.push(format!("la mia email è {e}"));
        }
    }
    let displayed = form.display_name.trim();
    if !displayed.is_empty() {
        clauses.push(format!("mi chiamo {displayed}"));
    }
    let nick = form.nickname.trim();
    if !nick.is_empty() {
        clauses.push(format!("mi chiamano anche {nick}"));
    }
    let birthday = form.birthday.trim();
    if !birthday.is_empty() {
        clauses.push(format!("sono nato il {birthday}"));
    }
    let address = form.address.trim();
    if !address.is_empty() {
        clauses.push(format!("vivo a {address}"));
    }
    let language = form.language.trim();
    if !language.is_empty() {
        clauses.push(format!("la mia lingua principale è {language}"));
    }
    let timezone = form.timezone.trim();
    if !timezone.is_empty() {
        clauses.push(format!("il mio fuso orario è {timezone}"));
    }
    let pronouns = form.pronouns.trim();
    if !pronouns.is_empty() {
        clauses.push(format!("i miei pronomi sono {pronouns}"));
    }
    let phone = form.phone.trim();
    if !phone.is_empty() {
        clauses.push(format!("il mio numero di telefono è {phone}"));
    }
    let occupation = form.occupation.trim();
    if !occupation.is_empty() {
        clauses.push(format!("lavoro come {occupation}"));
    }

    // The public block: identity clauses + the free-form `presentati` + the
    // always-on health/safety line — ALL under the public-consent prefix. The
    // primer is the public-profile channel (the form header says so): whatever
    // the user chooses to type here is public, health included. Anything they
    // want private they leave blank and add later through normal chat, which is
    // private by default.
    let mut public_block = String::new();
    if !clauses.is_empty() {
        public_block.push_str(&clauses.join(". "));
        public_block.push('.');
    }
    let presentati = form.presentati.trim();
    if !presentati.is_empty() {
        if !public_block.is_empty() {
            public_block.push_str("\n\n");
        }
        public_block.push_str(presentati);
    }
    let health = form.health_safety.trim();
    if !health.is_empty() {
        if !public_block.is_empty() {
            public_block.push_str("\n\n");
        }
        let _ = write!(
            public_block,
            "Una cosa importante di salute o sicurezza che un assistente \
             deve sapere sempre su di me: {health}."
        );
    }

    let mut out = String::new();
    if !public_block.is_empty() {
        out.push_str(PUBLIC_PROFILE_PRIMER_PREFIX);
        out.push_str("\n\n");
        out.push_str(&public_block);
    }
    out
}

/// Step 2 → `rules.md`. Turns the preset answers (Q1 sharing / Q2
/// exclusions / Q3 private topics / Q5 do-not-store) into imperative
/// policy sentences under [`ENGINE_RULES_SECTION_MARKER`]. Each is a
/// directive addressed to the memory engine, so the ingest LLM marks it
/// `engine_rule: true` and it lands in `rules.md`. Q4 (tone)
/// is a *behaviour* rule for the consumer agent, not an engine rule —
/// out of scope here (it belongs to the consumer-routing path).
fn compose_engine_rules_section(form: &ProfileSubmission) -> String {
    let mut rules: Vec<String> = Vec::new();
    match form.sharing_default.trim() {
        "private" => rules.push(
            "Di default tieni privati i miei fatti, visibili solo a me, a meno che \
             non sia chiaramente qualcosa pensato per essere condiviso."
                .to_owned(),
        ),
        "group" => rules.push(
            "Di default puoi condividere i miei fatti con il gruppo pertinente quando \
             ha senso: decidi tu come assistente."
                .to_owned(),
        ),
        "always_private" => rules.push(
            "Tieni sempre privati i miei fatti: non condividerli con nessun gruppo.".to_owned(),
        ),
        _ => {},
    }
    let excl = form.sharing_exclusions.trim();
    if !excl.is_empty() {
        rules.push(format!("Non condividere mai i miei fatti con: {excl}."));
    }
    let topics = form.private_topics.trim();
    if !topics.is_empty() {
        rules.push(format!(
            "Tieni sempre privati i temi che riguardano: {topics}."
        ));
    }
    let dns = form.do_not_store.trim();
    if !dns.is_empty() {
        rules.push(format!("Non memorizzare mai: {dns}."));
    }
    if rules.is_empty() {
        return String::new();
    }
    let mut out = String::from(ENGINE_RULES_SECTION_MARKER);
    for rule in rules {
        out.push_str("\n- ");
        out.push_str(&rule);
    }
    out
}

/// Step 3 → normal pipeline. The leftover low-weight preferences as plain
/// first-person prose under [`PREFERENCES_SECTION_MARKER`]; the ingest
/// LLM files them wherever it sees fit (no tag, no special routing).
fn compose_preferences_section(form: &ProfileSubmission) -> String {
    let mut clauses: Vec<String> = Vec::new();
    let color = form.favorite_color.trim();
    if !color.is_empty() {
        clauses.push(format!("il mio colore preferito è il {color}"));
    }
    let hobbies = form.hobbies.trim();
    if !hobbies.is_empty() {
        clauses.push(format!("i miei hobby e interessi sono {hobbies}"));
    }
    let food = form.food_preferences.trim();
    if !food.is_empty() {
        clauses.push(format!("per quanto riguarda il cibo: {food}"));
    }
    if clauses.is_empty() {
        return String::new();
    }
    let mut out = String::from(PREFERENCES_SECTION_MARKER);
    out.push_str("\n\n");
    out.push_str(&clauses.join(". "));
    out.push('.');
    out
}

fn render_form(user: &SessionUser, email: Option<&str>, ingest_available: bool) -> Html<String> {
    let email_value = email.unwrap_or("");
    let body = html! {
        h1 { "Welcome, " (user.sender_id) "!" }
        p.muted {
            "Let's give your memory a starting point in three steps. "
            "It's all optional — anything you leave blank doesn't go into the wiki, "
            "and you can always come back to it from the dashboard."
        }
        p.muted id="welcome-step-indicator" { "Step 1 of 3" }

        @if !ingest_available {
            (components::flash(
                "error",
                "The `llm.ingest` LLM slot is not configured in mwe-mcp.config.yaml. \
                 Save won't work until the slot is wired. \
                 You can press Skip for now to continue without a profile."
            ))
        }

        form id="welcome-form" action="/dashboard/welcome" method="post" {
            (step1_identity_fieldset(email_value))
            (step2_rules_fieldset())
            (step3_rest_fieldset())
        }

        // Two tiny vanilla listeners on the form. The stepper shows one
        // fieldset at a time (`[data-next]` / `[data-back]` are
        // `type=button`, never submit); the spinner swaps the clicked
        // submit's label while the POST is in flight. Both degrade to a
        // plain single-page form with JS off.
        script {
            (PreEscaped(WELCOME_STEPPER_JS))
        }
        script {
            (PreEscaped(WELCOME_SUBMIT_SPINNER_JS))
        }
    };
    Html(layout::authenticated_page("Welcome", user, &body))
}

/// Step 1 fieldset → `index.md`: the identity card plus the always-on
/// health/safety slot. A "Salta tutto" submit is repeated on every step
/// so the user can bail out at any point.
fn step1_identity_fieldset(email_value: &str) -> Markup {
    html! {
        fieldset data-step="1" {
            legend { "1 · Who you are" }
            p.muted {
                "Your identity and the things an assistant must "
                strong { "always" } " know: they go into your main page, "
                "the base context of every interaction."
            }
            p.flash.flash-info {
                "Everything you enter here — health & safety included — is "
                strong { "public" } " in your wiki, visible to other dashboard users. "
                "Write only what you're happy to share; "
                strong { "leave blank anything you want private" }
                " and tell your assistant later in normal chat, which keeps facts "
                "private by default."
            }

            // The email is the login identity, set by the admin at invite
            // and changeable only by the admin — so it is shown read-only
            // here. A read-only input still submits its value, so the email
            // still flows into the primer as a recorded fact; the user just
            // cannot diverge it from their account email.
            p {
                label for="email" { "Email" }
                input id="email" name="email" type="email" value=(email_value) readonly;
            }
            p.help.muted { "Your account email, set by your admin. Shown for reference — recorded with your profile." }

            div.field-grid {
                (components::text_field("display_name", "Full name", "text", "", false))
                (components::text_field("nickname", "Nickname", "text", "", false))
            }

            label for="presentati" { "Introduce yourself freely" }
            textarea id="presentati" name="presentati" rows="4"
                     placeholder="Tell us about yourself — whatever the fields below don't already capture." {}

            div.field-grid {
                (components::text_field("birthday", "Date of birth", "date", "", false))
                (components::text_field("address", "Address / city", "text", "", false))
                (components::text_field("language", "Primary language (it, en, …)", "text", "", false))
                (components::text_field("timezone", "Time zone (e.g. Europe/Rome)", "text", "", false))
                (components::text_field("pronouns", "Pronouns", "text", "", false))
                (components::text_field("phone", "Phone", "tel", "", false))
                (components::text_field("occupation", "Work / role", "text", "", false))
            }

            label for="health_safety" { "Health & safety (always relevant)" }
            textarea id="health_safety" name="health_safety" rows="2"
                     placeholder="severe allergies, celiac disease, chronic conditions, «only ever write to me in Italian»…" {}
            p.help.muted {
                "Things an assistant must keep in mind in every exchange, whatever the topic. "
                "Saved public like the rest of this step — leave it blank and mention it in "
                "chat later if you'd rather keep it private."
            }

            div.actions {
                button type="button" data-next="1" class="primary" { "Next →" }
                button type="submit" name="action" value="skip" class="secondary"
                       data-pending-label="One moment…" { "Skip all" }
            }
        }
    }
}

/// Step 2 fieldset → `rules.md`: the governance presets (Q1 sharing /
/// Q2 exclusions / Q3 private topics / Q5 do-not-store). Q4 (tone) is a
/// consumer-behaviour rule, out of scope here.
fn step2_rules_fieldset() -> Markup {
    html! {
        fieldset data-step="2" {
            legend { "2 · Your rules" }
            p.muted {
                "How the memory should treat your data: privacy, sharing, what not to store. "
                "Write them once here and they always apply."
            }

            fieldset.radio-group {
                legend { "By default, how do we handle sharing your facts?" }
                label {
                    input type="radio" name="sharing_default" value="private";
                    " Private by default — visible only to me, unless clearly meant to be shared."
                }
                label {
                    input type="radio" name="sharing_default" value="group";
                    " Shared with the relevant group when it makes sense — you decide, as the assistant."
                }
                label {
                    input type="radio" name="sharing_default" value="always_private";
                    " Always private — never share them with any group."
                }
            }

            div.field-grid {
                (components::text_field("sharing_exclusions",
                    "Who should it NEVER be shared with? (person or group)", "text", "", false))
                (components::text_field("private_topics",
                    "Topics to always keep private (e.g. health, money, work)", "text", "", false))
                (components::text_field("do_not_store",
                    "Things to NEVER store", "text", "", false))
            }

            div.actions {
                button type="button" data-back="1" class="secondary" { "← Back" }
                button type="button" data-next="1" class="primary" { "Next →" }
                button type="submit" name="action" value="skip" class="secondary"
                       data-pending-label="One moment…" { "Skip all" }
            }
        }
    }
}

/// Step 3 fieldset → normal pipeline: low-weight preferences. Carries the
/// final "Save and go" submit.
fn step3_rest_fieldset() -> Markup {
    html! {
        fieldset data-step="3" {
            legend { "3 · The rest" }
            p.muted {
                "A few preferences and interests, nothing binding. The memory files them "
                "wherever it makes most sense."
            }

            (components::text_field("favorite_color", "Favorite color", "text", "", false))

            label for="hobbies" { "Hobbies, interests, passions" }
            textarea id="hobbies" name="hobbies" rows="2" placeholder="cooking, running, photography, AI, music…" {}

            label for="food_preferences" { "Food preferences (tastes)" }
            textarea id="food_preferences" name="food_preferences" rows="2" placeholder="vegetarian, love spicy food, no fish…" {}

            div.actions {
                button type="button" data-back="1" class="secondary" { "← Back" }
                button type="submit" name="action" value="save" class="primary"
                       data-pending-label="Saving…" { "Save and go" }
                button type="submit" name="action" value="skip" class="secondary"
                       data-pending-label="One moment…" { "Skip all" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_emits_only_filled_clauses_in_first_person() {
        let form = ProfileSubmission {
            display_name: "Francesco".into(),
            favorite_color: "rosso".into(),
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(Some("franz@example.com"), &form);
        assert!(msg.starts_with(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
        assert!(msg.contains("la mia email è franz@example.com"));
        assert!(msg.contains("mi chiamo Francesco"));
        assert!(msg.contains("il mio colore preferito è il rosso"));
        assert!(!msg.contains("nickname"));
        assert!(!msg.contains("birthday"));
    }

    #[test]
    fn compose_joins_clauses_with_period_and_space() {
        let form = ProfileSubmission {
            display_name: "Alice".into(),
            occupation: "developer".into(),
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(None, &form);
        assert!(msg.starts_with(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
        assert!(
            msg.contains("mi chiamo Alice. lavoro come developer."),
            "{msg}"
        );
    }

    #[test]
    fn compose_appends_presentati_as_separate_paragraph() {
        let form = ProfileSubmission {
            display_name: "Alice".into(),
            presentati: "Lavoro nel mondo della pasta da vent'anni.".into(),
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(None, &form);
        assert!(msg.starts_with(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
        assert!(msg.contains("mi chiamo Alice."), "{msg}");
        assert!(msg.contains("\n\nLavoro nel mondo della pasta"), "{msg}");
    }

    #[test]
    fn compose_frames_health_public_under_the_prefix() {
        // The primer is the public-profile channel: health typed here is public
        // too, so it must sit inside the block the public-consent prefix covers.
        let form = ProfileSubmission {
            display_name: "Morgana".into(),
            health_safety: "sono celiaca".into(),
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(None, &form);
        assert!(msg.starts_with(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
        assert!(msg.contains("sono celiaca"), "{msg}");
        // The prefix opens the message and the health line follows it — i.e. the
        // health text is downstream of the public-consent sentence, not split off
        // into an un-framed trailer.
        let prefix_at = msg.find(PUBLIC_PROFILE_PRIMER_PREFIX).unwrap();
        let health_at = msg.find("sono celiaca").unwrap();
        assert!(
            health_at > prefix_at,
            "health must follow the public prefix: {msg}"
        );
    }

    #[test]
    fn compose_frames_health_public_even_when_it_is_the_only_field() {
        // Health alone still gets the public prefix (it used to be emitted as an
        // un-prefixed trailer when no identity clause was present).
        let form = ProfileSubmission {
            health_safety: "intollerante al lattosio".into(),
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(None, &form);
        assert!(msg.starts_with(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
        assert!(msg.contains("intollerante al lattosio"), "{msg}");
    }

    #[test]
    fn compose_returns_only_presentati_when_no_other_field_set() {
        let form = ProfileSubmission {
            presentati: "Una presentazione libera.".into(),
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(None, &form);
        assert!(msg.starts_with(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
        assert!(msg.ends_with("Una presentazione libera."), "{msg}");
    }

    #[test]
    fn compose_is_empty_when_no_field_at_all() {
        let msg = compose_ingest_message(None, &ProfileSubmission::default());
        assert!(msg.is_empty(), "{msg}");
        assert!(!msg.contains(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
    }

    #[test]
    fn compose_emits_public_consent_line_first() {
        let form = ProfileSubmission {
            display_name: "Francesco".into(),
            favorite_color: "rosso".into(),
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(Some("franz@example.com"), &form);
        // The primer prefix appears at index 0 and tells the LLM in
        // natural italian that the facts that follow are public — no
        // backticks, no JSON field names, no "owner_id" jargon. The
        // ingest system prompt maps "public" → `allow_ids: ["global"]`
        // (owner stays the subject), so we stay on the consumer-facing
        // side of the contract.
        assert!(msg.starts_with(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
        assert!(
            !msg.contains("`owner_id"),
            "primer must not leak JSON field names: {msg}"
        );
        assert!(
            !msg.contains("target_wiki_id"),
            "primer must not leak JSON field names: {msg}"
        );
        // The prefix carries the consent cue in plain italian.
        assert!(
            msg.contains("PUBBLICA") && msg.contains("chiunque"),
            "primer must signal public consent in plain italian: {msg}"
        );
        let prefix_end = msg
            .find(PUBLIC_PROFILE_PRIMER_PREFIX)
            .map(|i| i + PUBLIC_PROFILE_PRIMER_PREFIX.len())
            .unwrap();
        let after_prefix = &msg[prefix_end..];
        let email_idx = msg.find("la mia email è").expect("email clause present");
        let name_idx = msg.find("mi chiamo").expect("name clause present");
        assert!(
            email_idx > prefix_end,
            "email must follow the primer prefix: {msg}"
        );
        assert!(
            name_idx > prefix_end,
            "name must follow the primer prefix: {msg}"
        );
        // The prefix is separated from the field-derived clauses by a
        // blank line so the LLM reads it as an instruction paragraph,
        // not as part of the user's first-person prose.
        assert!(
            after_prefix.starts_with("\n\n"),
            "expected blank line after primer prefix: {msg}"
        );
    }

    #[test]
    fn step2_emits_engine_rules_section_with_bulleted_directives() {
        let form = ProfileSubmission {
            sharing_default: "always_private".into(),
            private_topics: "salute, soldi".into(),
            do_not_store: "il mio indirizzo di casa".into(),
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(None, &form);
        // The section marker frames the lines as governance rules, not
        // facts → the ingest LLM sets engine_rule:true → rules.md.
        assert!(msg.contains(ENGINE_RULES_SECTION_MARKER), "{msg}");
        // Each preset turns into a bulleted imperative directive.
        assert!(
            msg.contains("\n- Tieni sempre privati i miei fatti"),
            "{msg}"
        );
        assert!(
            msg.contains("\n- Tieni sempre privati i temi che riguardano: salute, soldi."),
            "{msg}"
        );
        assert!(
            msg.contains("\n- Non memorizzare mai: il mio indirizzo di casa."),
            "{msg}"
        );
        // No identity → no public-consent prefix leaks in.
        assert!(!msg.contains(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
    }

    #[test]
    fn step2_sharing_default_maps_each_preset() {
        let group = compose_engine_rules_section(&ProfileSubmission {
            sharing_default: "group".into(),
            ..ProfileSubmission::default()
        });
        assert!(group.contains("decidi tu come assistente"), "{group}");

        let private = compose_engine_rules_section(&ProfileSubmission {
            sharing_default: "private".into(),
            ..ProfileSubmission::default()
        });
        assert!(
            private.contains("Di default tieni privati i miei fatti"),
            "{private}"
        );

        // An exclusion alone (no sharing preset) still yields a rule.
        let excl = compose_engine_rules_section(&ProfileSubmission {
            sharing_exclusions: "il gruppo lavoro".into(),
            ..ProfileSubmission::default()
        });
        assert!(
            excl.contains("Non condividere mai i miei fatti con: il gruppo lavoro."),
            "{excl}"
        );
    }

    #[test]
    fn step2_is_empty_when_no_rule_answered() {
        // An unanswered sharing radio plus blank rule fields → no section.
        let section = compose_engine_rules_section(&ProfileSubmission::default());
        assert!(section.is_empty(), "{section}");
    }

    // (The former `health_safety_is_not_under_the_public_consent` test asserted
    // the opposite of today's design — the primer now frames health public too,
    // covered by `compose_frames_health_public_*` above.)

    #[test]
    fn health_follows_the_public_identity_block_when_both_present() {
        let form = ProfileSubmission {
            display_name: "Frodo".into(),
            health_safety: "allergia alle arachidi".into(),
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(None, &form);
        // Identity card stays public (under the prefix); health comes
        // after it as its own re-framed paragraph.
        assert!(msg.starts_with(PUBLIC_PROFILE_PRIMER_PREFIX), "{msg}");
        let name_idx = msg.find("mi chiamo Frodo").expect("name present");
        let health_idx = msg.find("allergia alle arachidi").expect("health present");
        assert!(health_idx > name_idx, "health must follow identity: {msg}");
    }

    #[test]
    fn three_steps_compose_three_marked_sections_in_order() {
        let form = ProfileSubmission {
            display_name: "Frodo".into(),        // step 1
            do_not_store: "i miei sogni".into(), // step 2
            favorite_color: "verde".into(),      // step 3
            ..ProfileSubmission::default()
        };
        let msg = compose_ingest_message(Some("frodo@example.com"), &form);
        let identity = msg.find(PUBLIC_PROFILE_PRIMER_PREFIX).expect("step 1");
        let rules = msg.find(ENGINE_RULES_SECTION_MARKER).expect("step 2");
        let prefs = msg.find(PREFERENCES_SECTION_MARKER).expect("step 3");
        assert!(
            identity < rules && rules < prefs,
            "sections out of order: {msg}"
        );
    }

    #[test]
    fn render_form_has_three_steps_and_the_stepper() {
        let user = SessionUser {
            sender_id: "frodo".into(),
            is_admin: false,
            session_jti: "test-jti".into(),
        };
        let html = render_form(&user, Some("frodo@example.com"), true).0;
        assert!(html.contains("data-step=\"1\""), "{html}");
        assert!(html.contains("data-step=\"2\""), "{html}");
        assert!(html.contains("data-step=\"3\""), "{html}");
        // The new step-1 health slot and step-2 governance presets are present.
        assert!(html.contains("name=\"health_safety\""), "{html}");
        assert!(html.contains("name=\"sharing_default\""), "{html}");
        assert!(html.contains("name=\"do_not_store\""), "{html}");
        // Both vanilla scripts ship.
        assert!(html.contains("welcome-step-indicator"), "{html}");
    }
}
