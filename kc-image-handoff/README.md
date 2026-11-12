# kc-image-handoff

A karucode plugin that lets a **text-only model see images**, working
entirely through karucode itself - and **only** a text-only model: a session
whose own model already has `vision = true` gets no note and no handoff. The agent calls
`plugin__image-handoff__see` with an image (a file path or a URL) and the
question it wants answered; the plugin creates a **temporary karucode
session**, pins it to a vision model from karucode's own provider
configuration, uploads the image, sends the question, and reads the answer
back off the event stream. The session never appears in any session list and
deletes itself once its run ends (older server builds get a durable session
the plugin deletes). No provider endpoint, model id or API key is handled by
the plugin: karucode's runtime is the only thing that talks to a model.

## 1. Tools

| Tool | What it does |
| --- | --- |
| `plugin__image-handoff__see` | One image, one question, one karucode session. `path` (workspace-relative or absolute) or `url` (http/https), plus `question`; optional `model` to name any karucode model for this call. Returns the vision model's answer plus metadata (model, provider, session id, run status, elapsed). |
| `plugin__image-handoff__status` | Read-only: the karucode server it drives, the vision model karucode's own provider configuration resolved, and the last few calls. |

### Vision models are handed nothing

The whole point is giving eyes to a model that has none. So the plugin checks
the **calling session's** model first (its pin, else the install's active
model) and its `vision` flag in karucode's provider configuration:

* **vision session** - the `before_prompt` note is suppressed (returning
  nothing to the host), and `see` refuses with a pointer to `read_file`,
  which already attaches the pixels natively. No session is spent, no image
  is ferried to another model.
* **text-only session** - the note tells it it has eyes available, and `see`
  runs the handoff.
* **undecidable** (unknown session, unreachable server, a model missing from
  the provider config) - the handoff still engages: undecidable is not "can
  see".

The lookup is cached per session id for 60s, so the per-turn note costs
nothing after the first.

### Long runs: `pending` + collect

A karucode vision run can outlive the 60-second budget the host gives one
plugin tool call. When that happens `see` does **not** fail: it returns

```json
{ "answer": null, "pending": true, "session_id": "s_...",
  "collect": "call `plugin__image-handoff__see` again with `session_id` ..." }
```

and the vision run keeps going in the background. The plugin process holds
one persistent event-stream connection, so the run's frames keep arriving
even after the temporary session retires itself. Call `see` again with that
`session_id` (and nothing else) to collect the answer; repeat while it
answers `pending`. A run that never finished within the wait window is
reported as an error only when its run ends in a failure.

## 2. Configuration: server + token

The plugin talks HTTP to the karucode server that spawned it (the loopback
default). Two settings matter:

| Key | Default | Meaning |
| --- | --- | --- |
| `server` | `http://127.0.0.1:8787` | The karucode server, plain HTTP. |
| `api_token` | — | Bearer token for the server. Prefer the `env` table (`IMAGE_HANDOFF_TOKEN`) over pasting one inline. |
| `api_token_env` | `IMAGE_HANDOFF_TOKEN` | Environment variable the token is read from. |
| `model` | — | Optional vision-model pin: a karucode model id (its provider is found automatically), with `provider` when ambiguous. Without it the plugin resolves: the active model if it has vision, else the first vision model of the active provider, else the first vision model anywhere. |
| `run_timeout_secs` | 55 | How long one `see`/collect call waits for the run before returning `pending`. Clamped 1..=600; keep it under the host's 60s tool budget. |
| `max_image_mb` | 8 | Largest image accepted. The server's upload ceiling is 8 MiB. |
| `max_output_chars` | 12000 | Answer length bound. |

The host clears a plugin's environment and holds the auth token itself, so
the token is passed through the operator's `[[plugins]]` entry:

```toml
[[plugins]]
name = "image-handoff"
command = ["/home/karutoil/karucode-plugins/kc-image-handoff/target/release/image-handoff"]
enabled = true
permissions = ["files.read", "network"]

[plugins.settings]
server = "http://127.0.0.1:8787"

[plugins.env]
# The automation API key (0600 config file, same owner as this file).
IMAGE_HANDOFF_TOKEN = "kck_..."
```

## 3. Behaviour you can rely on

* **Image formats:** PNG, JPEG, GIF, WebP, BMP — decided by magic bytes, never
  by the extension. A file named `.png` that is not a PNG is refused. SVG is
  deliberately unsupported (not a raster vision input, and an executable XML
  document should not be laundered to a remote parser).
* **URL fetches:** only `http://`/`https://`, bounded by `max_image_mb` and
  a short fetch timeout; a non-image `Content-Type` is refused rather than
  uploaded.
* **No hidden retries:** a refused create, upload or message surfaces the
  server's own error body, and a run that ends in a failure is reported with
  the run's status and error. A run that is merely slow is `pending`, never
  an error.
* **Honest empties:** a vision model that answers only in a reasoning channel
  is reported as such, not as a blank success.
* **Answers are a model's report, not pixels.** The `before_prompt` note tells
  the calling model exactly that, so it treats the answer with the same
  skepticism as any other model output.

## 4. Tests

`./check.sh` runs fmt, clippy `-D warnings`, the unit suite, the integration
suite and a release build.

- **Unit** (68): HTTP client parsing, settings clamps, the vision-model
  resolver (active-model / first-vision / per-call override / errors that
  name what to configure), the attachment-block format.
- **Integration** (7 + 1 live): the real plugin binary over stdio against a
  scripted karucode API server - the ephemeral flow (create-with-pin + Bearer
  token + raw image upload + composer-format message + SSE frames read, and
  *no* delete because the ephemeral session retires itself), the
  old-server fallback (probe refused with 400, retry, and a `DELETE` that
  must happen), a per-call model pin, the pending/collect round trip, tool
  listing, and the vision gate both ways: a session pinned to a vision model
  gets its `see` refused with a `read_file` pointer and a null
  `before_prompt`, while a text-only session gets the note and the handoff. The fake server accepts connections in any order (the
  plugin's persistent reader races the flow for the backlog), so it routes
  by request line with per-connection threads.
- **Live** (`--ignored`, real karucode server): draws a PNG with known text
  and requires the vision model to read it back through a real session,
  handling `pending` by collecting with the session id. Verified end to end:
  pending at 55s, collected at 212s, exact transcription.

## 5. Install / upgrade

```sh
./install.sh                        # build + write the data-dir manifest
curl -s -X POST localhost:8787/api/plugins/reload \
  -H 'X-Karucode-Client: web' -H 'Content-Type: application/json' -d '{}'
```

After a karucode upgrade, refresh the vendored SDK and rebuild:

```sh
karucode plugin sdk . && ./check.sh && ./install.sh
curl -s -X POST localhost:8787/api/plugins/image-handoff/reload \
  -H 'X-Karucode-Client: web' -H 'Content-Type: application/json' -d '{}'
```

With the default (karucode-config) resolution there is no key to pass at all;
`install.sh` copies the keyless manifest as-is. Only an explicit `base_url`
override needs a key (env table or 0600 `api_key_file`).
