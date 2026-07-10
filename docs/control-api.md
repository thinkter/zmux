# zmux control API v1

The control API is a transport-independent JSON protocol for local automation.
Each request is one UTF-8 JSON frame, no larger than 64 KiB. Implementations
may use one request per connection or newline-delimited frames; responses end
with a newline for shell-friendly use.

Every mutation names the workspace and surface it should affect. Clients must
not rely on the currently focused terminal because focus can change while a
request is queued.

```json
{"version":1,"id":7,"method":"surface_send_input","params":{"workspace_id":12,"surface_id":99,"input":"cargo test -j 6\n"}}
```

Successful responses use `status: "ok"`; failures use `status: "error"` with a
typed `code`, a human-readable `message`, and `retryable`. `discover` returns
the capability set so clients can degrade safely when screenshots, browsers, or
other optional surfaces are unavailable.

## Optional browser surfaces

Browser support is an opt-in `browser` Cargo feature. A terminal-only build has
no WebView dependency and reports `browser_surfaces: false`. Enabling the Rust
feature only exposes the browser abstraction; it does not claim that a native
WKWebView, WebView2, or WebKitGTK adapter is installed. Inspect the detailed
`browser.backends` values from `discover` and create a surface only when a
backend reports `status: "available"`.

Browser surfaces use the same `workspace_id` and stable `surface_id` as other
surfaces, so a workspace host can attach them to a tab or split without relying
on a transient UI entity ID. `surface_create_browser` carries an explicit
policy: sessions default to ephemeral, permissions default to deny, and
downloads default to deny. A persistent profile must name its own storage path;
a permitted download must name an approved directory.

For automation, request `browser_accessibility_snapshot` first and send the
returned `snapshot_node` target to `browser_interact`. Every target is pinned
to a `document_id`; a navigation makes an old target fail with `stale_target`
rather than accidentally matching a similar element on the new page.
Accessibility and CSS targeting also require the expected document ID.

Browser automation methods are `browser_get_info`, `browser_navigate`,
`browser_accessibility_snapshot`, `browser_interact`,
`browser_evaluate_javascript`, `surface_screenshot`, `browser_console_list`,
`browser_cookie_list`, `browser_storage_state`, and `browser_download`.
`timeout_ms` remains capped at 30 seconds, and scripts, snapshots, screenshots,
console entries, cookies, and storage responses have protocol-level limits.
Errors are typed (`stale_target`, `permission_denied`, `download_denied`,
`navigation_failed`, `javascript_failed`, `timeout`, and `overloaded`) rather
than requiring clients to parse backend text.

Supported methods in v1 are:

- `discover`
- `workspace_list`, `workspace_create`, `workspace_select`,
  `workspace_rename`, and `workspace_close`
- `surface_list`, `surface_create_terminal`, `surface_focus`, `surface_split`,
  `surface_close`, `surface_reorder`, `surface_send_input`,
  `surface_read_screen`, and `surface_screenshot`
- When the browser capability is available: `surface_create_browser`,
  `browser_get_info`, `browser_navigate`, `browser_accessibility_snapshot`,
  `browser_interact`, `browser_evaluate_javascript`, `browser_console_list`,
  `browser_cookie_list`, `browser_storage_state`, and `browser_download`
- `notification_list`, `notification_create`, `notification_acknowledge`, and
  `notification_clear`

Requests have a bounded optional `timeout_ms` (capped at 30 seconds), and
screen content is bounded by the implementation's advertised limit. Local IPC
authentication and endpoint discovery are owned by the transport layer.
