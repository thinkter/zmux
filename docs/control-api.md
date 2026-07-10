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

Supported methods in v1 are:

- `discover`
- `workspace_list`, `workspace_create`, `workspace_select`,
  `workspace_rename`, and `workspace_close`
- `surface_list`, `surface_create_terminal`, `surface_focus`, `surface_split`,
  `surface_close`, `surface_reorder`, `surface_send_input`,
  `surface_read_screen`, and `surface_screenshot`
- `notification_list`, `notification_create`, `notification_acknowledge`, and
  `notification_clear`

Requests have a bounded optional `timeout_ms` (capped at 30 seconds), and
screen content is bounded by the implementation's advertised limit. Local IPC
authentication and endpoint discovery are owned by the transport layer.
