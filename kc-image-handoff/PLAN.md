# kc-image-handoff — build plan

Purpose: let a text-only model "see" images. The agent calls
`plugin__image-handoff__see(path_or_url, question)`; the plugin reads the
image file (or fetches the URL), base64s it, sends it to a **configured
vision model** (OpenAI-compatible /v1/chat/completions, image_url parts),
and returns the vision model's answer to the asking model.

## Layout
- `src/main.rs` — tool wiring (see, status/config tool), before_prompt hook
- `src/config.rs` — Settings: base_url, model, api_key / api_key_env /
  api_key_file, timeout_secs (default 55, clamp 1..=300), max_image_mb
  (default 20, clamp 1..=100), max_output_chars (default 12000, clamp
  1000..=20000), system_prompt (default vision-assistant contract), detail.
- `src/media.rs` — image loading + sniffing (PNG/JPEG/GIF/WebP/BMP magic
  bytes), extension fallback, size guards, mime mapping.
- `src/vision.rs` — API client (ureq 2 + rustls), request build, response
  parse, error mapping. Non-streaming, single attempt, honest errors.
- `vendored sdk.rs/protocol.rs` (from `karucode plugin init`, refreshed by
  `karucode plugin sdk .`)

## Tools
- `see` — path (workspace-relative or absolute) OR url, question. Returns
  answer text + meta (model, source kind, bytes, truncated?).
- `status` — read-only: show resolved settings (key redacted), endpoint
  reachability is NOT probed (no side effects); lists model.

## before_prompt hook
One stable static block (no timestamps!) naming the tool and when to use it
(prompt-cache friendly). Capped well under 4 KiB.

## Key handling (plugin env is cleared!)
`api_key_env` (default OMNIROUT... no: default `IMAGE_HANDOFF_API_KEY`) →
`KARUCODE_PLUGIN_SETTINGS` api_key → `api_key_file` (read at call time, 0600
required warn). Loopback endpoints may omit the key.

## Live setup for this host
Omniroute local: http://127.0.0.1:20128/v1 (no Cloudflare), public
https://omni.karutoil.site/v1 (Cloudflare; ureq/rustls TLS passed it in the
probe). Live key: karucode DB `model.providers[0].api_key` — pass via
plugin `env` table in config.toml (env wins over settings; KARUCODE_PLUGIN_*
vars are allowlisted through).

## Tests
- unit: settings parsing/clamps/redaction, media magic sniff, request JSON
  shape, response parse, error mapping
- stdio integration: run the binary against a scripted HTTP server (std TcpListener
  in a thread) — full tool call → answer
- manifest/schema assertions (like deep-research's tool_surface tests)
- live (ignored by default, `cargo test -- --ignored` when env set): real
  Omniroute call with a generated PNG containing known text

## Verification bar
cargo build --release, fmt, clippy -D warnings, cargo test; then install
manifest via data dir + POST /api/plugins/reload; drive the tool from a live
session; live E2E with a text-bearing image.
