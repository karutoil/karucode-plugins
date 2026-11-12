# The plugin UI gap — closed

This file used to document what the plugin system could not do: a plugin could
add tools and hooks but not a panel, and `tools/call` carried no session or
workspace, so a workspace-scoped plugin had to guess its project from the last
`before_prompt`.

**Both are fixed.** karucode now supports plugin panels and forwards call
context. This file is kept short because the gap is closed, and it is worth
recording what changed so nobody re-solves it.

## What changed upstream

### 1. Panels

A plugin declares a panel in its manifest and answers `panels/read`; the host
forwards the JSON payload untouched and the frontend draws it with a first-party
component (or as read-only JSON when the app has no renderer for it).

```json
{"id":"todo","title":"Todo","icon":"check","source":"static","refresh_ms":0}
```

* the id is hosted as `plugin__<plugin>__<id>`, so it cannot collide with a
  built-in tab or another plugin's panel;
* the tab is listed in `GET /api/config` under `plugin_panels` and read over
  `GET /api/plugins/:name/panels/:panel?project=&session=`;
* a plugin still cannot ship markup or script — it ships **data**. That keeps
  the CSP and `frame-ancestors 'none'` meaningful, which is why this is the shape
  the feature took rather than iframes.

`kc-todo` uses the SDK's `.panel_with("todo", "Todo", Some("check"), 0, …)`.

### 2. Call context

`tools/call` now carries
`context: {session_id, run_id, agent_id, workspace, project, depth}`, exposed as
`ToolRequest::context()` / `::workspace()` / `::session_id()`.

This deleted the caveat this plugin shipped with. Workspace resolution is now:

1. an explicit `workspace` argument,
2. `context.workspace` from the host,
3. `settings.workspace` (a pin, for hosts too old to send context),
4. the workspace remembered from the last `before_prompt`.

Two sessions in two projects no longer cross-talk. `src/session.rs` has tests for
the precedence order, and `workspace()` returning `NoWorkspace` still fails with
a message naming the fix rather than silently guessing.

## What is still true

A plugin *cannot* render its own UI. The panel is a tab whose **contents** the
app draws from the plugin's data. So the nicer view is still a first-party
component — it just lives in `ui/TodoPanel.tsx` here and is registered with
`registerPluginPanelRenderer`, instead of the whole tab being a core patch. See
README §4.

Both changes are additive: a plugin that declares no panel and ignores `context`
behaves exactly as before, and the protocol version did not change.
