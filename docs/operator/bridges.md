# Bridges — wiring a consumer

**Nav: Bridges.** `/dashboard/bridges`

A bridge connects a **consumer** — the bot or assistant that talks to this
memory — to mwe-mcp. The catalog covers three hosts today, each with a page
for you and a machine-readable `install.md` you can hand to a capable
consumer instead of following it yourself.

| Consumer | What it is |
|---|---|
| **NanoClaw (nanoco)** `nanoclaw` | The ready-made assistant. Start here if you have no consumer of your own. |
| **Hermes (Nous Research)** `hermes` | A standard consumer, wired at full fidelity. |
| **Claude Code (Anthropic)** `claude-code` | A smart consumer: brings its own model and authors a project's wiki. |

The claude.ai web app is on the same page and needs no bridge at all.

## The ready-made assistant, in one command

`/dashboard/bridges/nanoclaw` gives you a single line to run:

```
curl -fsSL https://<your server>/bridges/nanoclaw/install.sh | sh
```

It does everything except the token, which it leaves you to mint and paste.
The button **Create an install command that carries the token** gives you the
same command with a one-time claim in it: the claim is not the token, it is
good once, it expires in fifteen minutes, and it is spent the moment the
installer trades it for a token of its own. That token belongs to one fixed
consumer, so running the installer a second time refreshes that consumer
rather than coining a second identity and a second wiki.

The installer finds your NanoClaw or clones one at the tested revision into
`~/nanoclaw` (`NANOCLAW_DIR` puts it elsewhere or points at your own fork),
places the mwe agent template and the memory skill inside it, runs NanoClaw's
own setup, stamps the agent, installs Telegram and wires your own chat to the
agent, restarts the service, and then watches for your first message to
confirm the turn was stored and recalled.

Two things only you know, and it asks for them: the bot token @BotFather gave
you, and your own numeric Telegram id. Both can come from the environment
instead — `MWE_TELEGRAM_BOT_TOKEN` and `MWE_TELEGRAM_OPERATOR_ID`.

**Run it in a terminal you are sitting at** — NanoClaw's own two questions and
the browser sign-in need one, and the installer stops and says so rather than
hanging. Re-running the whole
command is how you update an install. On Windows, NanoClaw runs under WSL2, so
open the WSL2 shell and run it there.

### Who the assistant speaks for

One line per person in `senderMap` in the fork's `mwe.json` — their
`<channel>:<platform id>` mapped to their user id in this memory — and a tick
for each of them, plus **guest**, in the consumer's delegations on the
[Tokens](tokens.md) page. The installer writes the first line, yours; the rest
are added the same way as people arrive. Anybody the map does not name speaks
as a guest, whose turns recall only public memory and store nothing. There is
no falling back to the owner.

## Hermes

Run the installer from inside your `hermes-agent` checkout — the page gives
the `sh` and the PowerShell form — and it places the memory, media and
watchdog plugins, the reverse-channel hook and the daily-digest script. Then
three things are yours:

1. Issue a **standard** consumer token on the [Tokens](tokens.md) page and set
   it as `MWE_TOKEN` in hermes's `.env`.
2. Set `memory_enabled: false` and `user_profile_enabled: false` in hermes's
   `config.yaml`, so this memory is its only memory.
3. Enable the hook plugins you want and restart hermes.

## Claude Code

No plugins and no token. Register the server once:

```
claude mcp add --transport http mwe-mcp https://<your server>/mcp --scope user
```

then run `/mcp` in a session and approve the sign-in in the browser. On
approval a dedicated wiki is created for the connection. The page also carries
the **session-start hook** it recommends strongly, and the one-line override
that keeps a given repository out of the memory entirely.

## The claude.ai web app

It connects as a smart consumer over OAuth, with no bridge and no token. In
claude.ai, open **Settings → Connectors** and choose **Add custom connector**,
paste this server's MCP URL, and approve the sign-in when it sends you back
here. A dedicated wiki is created for it on approval. The page also links the
skill to upload, so the app knows how to use the memory well.

Approved connections, and the button that disconnects one, live on the
[Tokens](tokens.md) page.
