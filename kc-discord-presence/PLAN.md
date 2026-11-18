# kc-discord-presence — plan and outcome

Goal: a karucode plugin that shows what the operator is doing in karucode
(project, session, run state, approvals) as **Discord Rich Presence** on their
profile, with setup that is one paste and no bot.

## Decisions (as built)

- **Rich Presence over the local client's IPC socket** — the profile line
  ("Playing <app>", project, session, elapsed), which is what "rich presence"
  means. No bot, no token, no gateway, no invite: the only input is a public
  Application ID, and the only requirement is that the Discord desktop client
  runs on this machine.
- **A channel webhook is an optional second sink**, not the product: the same
  state as a card edited in place, plus notifications. It exists because Rich
  Presence cannot reach a Discord that is only in a browser or on a phone.
- **State source = karucode's own SQLite DB, read-only.** No API key, no auth
  token, no second credential to expire. The host's env allowlist gives a plugin
  `HOME`, so `~/.local/share/karucode/karucode.db` resolves; a `data_dir` setting
  covers a custom `--data-dir`. Queries are defensive: a missing table or column
  degrades the presence and is reported in `discord_status`/the panel.
- **Live detail = host hooks, not polling the transcript.** `on_tool_result`,
  `on_run_end`, `on_notify` only update memory and wake the publisher (hooks have
  a 5 s budget).
- **Setup through chat.** `discord_configure` validates a pasted Application ID,
  connects, publishes the presence, and persists the ID, so the operator never
  edits a config file. A closed Discord does not lose the paste: the ID is stored
  and the presence attaches when a client appears.
- **No secrets in logs or panels.** The Application ID is public; the optional
  webhook token is redacted everywhere, including inside ureq transport errors
  that quote the URL; the state file is written 0600.

## Layout

- `src/presence.rs` — IPC framing, socket discovery, `SET_ACTIVITY`, reconnect,
  byte-exact clamping, activity built from the snapshot.
- `src/config.rs` — settings + persisted state (Application ID, webhook, card id).
- `src/karucode.rs` — read-only DB snapshot: running runs, recent sessions,
  approvals, agents, recent outcomes.
- `src/card.rs` — snapshot → Discord embed for the optional card.
- `src/discord.rs` — webhook client: POST/PATCH/DELETE, 429, 404 by verb, token
  redaction.
- `src/worker.rs` — one publisher loop for both sinks.
- `src/main.rs` — plugin wiring: 4 tools, 1 panel, 1 button, 3 hooks.
- `tests/` — fake `discord.com`, fake `discord-ipc-0`, fixture DB, protocol e2e.

## Verified

1. `./check.sh` — 114 unit tests + 4 end-to-end protocol tests, clippy with
   `-D warnings`, vendored SDK matches the installed karucode build.
2. Presence end-to-end: the real binary handshakes with the Application ID over a
   real Unix socket, sends `SET_ACTIVITY` with project/session/timestamp, reports
   `connected: true`, shows `Presence: live` in the panel, and on shutdown sends
   a null activity plus `CLOSE`.
3. Real Discord API (card path): a bogus webhook returned
   `Discord refused the message (404, code 10015): Unknown Webhook`.
4. Real karucode DB: the activity rendered this very session (project
   `karucode-plugins`, running, model, also-running, last-hour outcomes).
5. Installed into the running server: `ready`, 4 tools, panel
   `plugin__discord-presence__board`, button `plugin__discord-presence__push`.
6. Real browser: the Discord inspector tab renders the presence state and the
   profile lines; "Push to Discord" produces a toast.

## Bugs found and fixed during verification

- A `404` on webhook **post** was reported as "the message was deleted"; it means
  the webhook is unknown. Split the mapping by verb.
- `discord_status` reported no snapshot until the first publish; it reads live.
- ureq transport errors quote the request URL, token included; errors are now
  redacted at the client boundary.
- The panel's `list` view is plain text, so card markdown leaked `**`/backticks.
- A config test read the machine's real state file; made hermetic.
- `filter().next_back()` in three tests (clippy) and a duplicate-module lint on
  the shared test support module.

## Left for the operator

Create a Discord application (name = the profile line), copy the Application ID,
and paste it into a chat. Until then the panel honestly says `Presence: off — set
an Application ID with discord_configure`.

No Discord desktop client is running on this box, so the live attach will happen
when it starts; the fake IPC server in the tests is what proves the protocol.
