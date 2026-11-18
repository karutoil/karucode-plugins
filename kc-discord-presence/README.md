# kc-discord-presence

What karucode is doing, as **Discord Rich Presence** on your profile:

```
Playing karucode
karucode-plugins
build the discord plugin
01:43:12 elapsed
```

The first line is the name of the Discord application you point it at, the second
is the project, the third is the session (or the approval that is waiting on
you), and the timer is the run's elapsed time. It is driven by karucode's own
state, so it changes as you work and disappears when the plugin stops.

Optionally the same state can also be mirrored into a channel as a live card,
with notifications - see [Channel card](#channel-card-optional). The presence
needs no webhook; the card needs no Application ID.

## Setup is one paste

1. Open <https://discord.com/developers/applications> → **New Application** →
   name it (that name is the "Playing …" line, e.g. `karucode`) → open it and
   copy the **Application ID**. It is a long number, **not** the client secret.
2. In a karucode chat: *"set up the discord presence with application id
   &lt;paste&gt;"*.

The agent calls `discord_configure`, which stores the ID, connects to the Discord
desktop client on this machine and publishes the presence. If the client is not
running, the ID is still stored and the presence attaches within 30 seconds of it
starting - you do not have to repeat the paste.

There is no bot, no token, no gateway connection, no invite and no privileged
intent. The Application ID is **public**: it names the presence, it does not
authorize anything.

**The Discord desktop client must be running on the machine that runs karucode.**
Rich Presence is a local socket protocol; a Discord that lives only in a browser
or on a phone cannot see it. If that is your setup, use the
[channel card](#channel-card-optional) instead - it is the same state, delivered
over HTTPS.

## What it shows

| karucode state | Profile line |
| --- | --- |
| a run is going | `details`: project · `state`: session title · elapsed timer from the run's start |
| an approval is pending | `state`: `⏸ write_file docs/plan.md` - what needs you outranks the title |
| subagents are running | `state`: `session title · 2 agents` |
| nothing is running | `details`: `idle` · `state`: `last: <session>` - no timer |
| the database cannot be read | `details`: `no data` · `state`: the reason |

Discord's two detail lines are capped at 128 **bytes** each, so a long title is
cut on a character boundary rather than rejected. The presence is only re-sent
when it changes, and never more than once per 15 seconds - the client silently
drops updates that arrive faster.

## Tools

| Tool | What it does |
| --- | --- |
| `discord_status` | Is the presence live, on which socket, what does it say, what was the last error; the optional card's state; whether karucode's database is readable. Sends nothing. |
| `discord_configure` | Store an Application ID (presence) and/or a webhook URL (card), connect, publish. Optional `large_image`, `large_text`, `presence_socket`, `refresh_secs`, `notify`, `thread_id`. |
| `discord_publish` | Push the current state now: presence first, then the card. |
| `discord_notify` | Post a one-off message to the channel (needs a webhook). |

## In the browser

* **Inspector → Discord** (plugin panel, polled every 5 s): the presence state
  first (`live` / `waiting` / `off`, with the socket and the error verbatim),
  then the optional card, then what the profile line currently says.
* **Push to Discord** in the inspector header pushes both now.

## Channel card (optional)

Give `discord_configure` a webhook URL as well - Discord: *Edit Channel →
Integrations → Webhooks → New Webhook → Copy Webhook URL* - and the same state is
published as a message that is edited in place, plus the notification kinds you
choose (`notify`, default `needs_input,error`). That is the surface that works
when Discord is not running on this machine, and the one that can reach your
phone.

The card is not destructive to reconfigure: the same webhook reuses the message
already in the channel, a move to a different webhook deletes the old card, and a
restart reuses the persisted message id instead of posting a second one.

## Settings

| Key | Default | Meaning |
| --- | --- | --- |
| `application_id` | - | The Discord Application ID the presence is published under. Usually set by `discord_configure`. |
| `presence` | `true` | Switch the profile presence off without forgetting the ID. |
| `large_image` | - | Rich Presence asset key (uploaded to the application) or an `https` image URL. |
| `large_text` | - | Tooltip for the large image (128 bytes). |
| `presence_socket` | discovered | An explicit `discord-ipc-N` path, for a client that is not where discovery looks. |
| `webhook_url` | - | Optional channel webhook. |
| `thread_id` | - | Post the card into one thread of that channel. |
| `refresh_secs` | `20` | Seconds between card edits, floor 10. `0` turns the card off. The presence is unaffected. |
| `notify` | `needs_input,error` | Comma list of `run_finished`, `needs_input`, `error`, `info`, or `all` / `none`. |
| `username` / `avatar_url` | `karucode` / - | The webhook's display name and avatar. |
| `include_recent` / `max_sessions` | `true` / `3` | What the card may show. |
| `data_dir` | `$HOME/.local/share/karucode` | Only for a custom `--data-dir`: a plugin cannot see `KARUCODE_DATA_DIR`. |
| `state_file` | `<data_dir>/discord-presence/state.json` | Where the ID and the webhook live (mode 0600). |
| `api_base` | from the URL | Discord REST base override for the card (tests, or a proxy). |

## How it works

* **Presence**: the Discord desktop client listens on a Unix socket
  (`discord-ipc-0` … `-9`) in the runtime directory. The plugin speaks its
  framing - a 4-byte little-endian opcode, a 4-byte length, then JSON - sends
  `HANDSHAKE` with the Application ID, then `SET_ACTIVITY` with the two lines and
  the timestamp. Discovery covers `$XDG_RUNTIME_DIR`, `/run/user/<uid>`, the
  flatpak and snap subdirectories, `$TMPDIR` and `/tmp`. A missing client is
  retried every 30 s; a dead socket is dropped and reconnected; shutdown sends
  `SET_ACTIVITY` with a null activity and the protocol's `CLOSE`, so nothing is
  left stuck on your profile.
* **State**: karucode's own SQLite database, read read-only (`query_only`, busy
  timeout). Every query is defensive - a missing table or column degrades the
  presence and is reported in `discord_status` and the panel instead of failing.
  No API key is needed, which is why setup is one paste.
* **Live detail**: the host's `on_tool_result`, `on_run_end` and `on_notify`
  hooks only record and wake the publisher; the publisher is the only thing that
  talks to Discord.

## When it goes wrong

* **No Discord client** - `discord_status` and the panel say so, with the
  directories that were searched, and the ID stays stored.
* **Bad Application ID** - Discord answers with an `ERROR` frame and the message
  is Discord's own (`code 4000: Invalid Client ID`), not a generic failure.
* **The client restarts** - the next write fails, the socket is dropped and the
  presence reconnects on the next attempt.
* **`429` on the card** - the publisher parks for `retry_after` instead of
  retrying in a loop.
* **The card was deleted by hand** - the next publish notices the `404`, forgets
  the id and posts a new one.

## Security

* The **Application ID is public** - it names the presence, it authorizes
  nothing. There is no secret in the presence path at all.
* A webhook URL *is* a credential: it is written to `state.json` with mode
  `0600`, and every surface that shows it - logs, `discord_status`, the panel,
  error messages, including transport errors that quote the URL - shows
  `…/webhooks/<id>/***`.
* Card messages use `allowed_mentions: {"parse": []}`, so a session title
  containing `@everyone` cannot ping the channel.
* Every string from karucode is clamped to Discord's limits before it is sent.

## Develop

```bash
cargo test                 # 114 unit tests + 4 end-to-end protocol tests
cargo clippy --all-targets -- -D warnings
./check.sh                 # the above, plus the vendored-SDK guard
./install.sh --reload      # build, register, rescan (needs KARUCODE_TOKEN)
```

The end-to-end tests spawn the real binary, speak the host's JSON-RPC to it, and
point it at a temporary karucode database, a scriptable stand-in for
`discord.com` **and a fake `discord-ipc-0`** that speaks the same framing the
real client does - so the handshake, the activity payload, the reconnect, the
rejection and the shutdown-clear are all exercised without Discord installed.

Linux and macOS only: the presence uses Unix sockets, and the state file uses
Unix permissions.

## Not in scope

* **Bot presence** (a bot user's "Playing …" in a member list). It needs a bot
  token and a gateway websocket; Rich Presence gives the same line on *your*
  profile with no credential at all.
* **Which tab you are looking at.** The server cannot see your browser's focus,
  so "what I am doing" means what is running and what changed most recently -
  which is also the honest answer.
