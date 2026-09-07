# packaging

Service definitions for running `mwe-mcp` as a background daemon that
restarts on failure and comes back after a reboot, under an account nobody
logs in with.

| Platform | File | Registered with |
|---|---|---|
| macOS | [`macos/com.mwe-mcp.server.plist`](macos/com.mwe-mcp.server.plist) | `launchctl bootstrap system …` |
| Windows | [`windows/mwe-mcp-task.xml`](windows/mwe-mcp-task.xml) | `schtasks /create /xml …` |

**Linux has no file here** because it does not need one: run `mwe-mcp serve`
in a terminal and it offers to provision the dedicated `mwe-mcp` account,
lock the workdir to it, and install and start the systemd unit. The unit is
rendered by the binary (`service_unit` in `crates/mwe-mcp-server/src/main.rs`),
so it always agrees with the binary that will run it.

The two files here are edited by hand instead, and the step-by-step that
provisions the account and the directories around them is in
[`INSTALL.md`](../INSTALL.md#run-it-as-a-service). Adjust the port and the
bind address to your topology; every other value matches the paths INSTALL.md
provisions.

**Why an account nobody logs in with.** The per-reader redaction is applied
when the server renders an answer, but the markdown and `engine.db` under the
workdir are cleartext on disk. Anything running as an account that can read
those files reads the un-redacted union of every fragment — which is the whole
governance, bypassed. Giving the daemon its own account, and the workdir to
that account alone, is what keeps a co-located agent's file tools out.
