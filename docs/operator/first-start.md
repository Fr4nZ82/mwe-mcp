# First start

You have the binary and the server is running — if not,
[`INSTALL.md`](../../INSTALL.md) covers getting it and running it as a service.
This page is the first ten minutes in the browser.

## Create the admin account

Open **`http://127.0.0.1:8742/dashboard/setup`**. The page says *"This
deployment is empty, so the dashboard is asking you to create the single admin
account that will manage it from now on."* There is exactly one admin per
deployment.

Fill in:

- **Email** — used to sign in, and to send you a recovery link once an
  outgoing mail server is set up.
- **Admin id** — lowercase letters and digits, starting with a letter; no
  underscore, no hyphen. This is the name the memory calls you by **and** the
  name of your own memory wiki, so pick the one you want to keep.
- **Password** and **Confirm password** — minimum 12 characters.

Press **Create admin**. You are signed in and taken straight to the model
slots, because that is what the memory needs next.

## Fill the six model slots

The page you land on is **LLM config**, and it opens with a red line naming
every slot with no model behind it. Do this now: without all six the memory
does not work, and the same line follows you onto the Home page until it is
gone. See [The six model slots](model-slots.md).

## Then, in this order

1. **[Server settings](settings.md) → Public address of this server.** Three
   things are built on it and none of them works without it: the
   password-reset link, the invitation email, and the link a consumer mints so
   somebody can open their own memory.
2. **[Users](users.md)** — one account per person, each with an invitation
   link you hand over.
3. **[Groups](groups.md)** — if several people are going to share memory.
4. **[Tokens](tokens.md) and [Bridges](bridges.md)** — the credential a
   consumer holds, and how to wire it up. If you have no consumer of your own,
   the Bridges page installs the ready-made assistant with one command.

## Your own memory, too

The admin is also a person here. The first time you open **Home** the
dashboard sends you to the three-step **Welcome** wizard, which seeds your
identity card, your standing rules and a few preferences. All of it is
optional, and **Skip all** is a real answer — see
[Your first sign-in](../user/first-sign-in.md), which is the same wizard from
the other side.

## Where everything lives

Everything the server owns is inside one folder, the workdir: `engine.db`,
the Markdown memory under `wikis/`, the prompt overrides under `prompts/`,
the logs under `logs/`, `mwe-mcp.config.yaml` and `mwe-mcp.env`. Back up that
folder and you have backed up the deployment — [Backup](backup.md) does it
from the dashboard. [`INSTALL.md`](../../INSTALL.md) has the full layout and
the permissions it wants.
