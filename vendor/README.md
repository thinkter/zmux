# Vendored terminal integration patches

These crates are narrow, source-pinned patches used to surface terminal
notification escape sequences through Zed's existing title-change event path
and to make every Zed-owned terminal creation path install zmux's per-terminal
notification capability before the terminal is mounted.

- `vte/` is crates.io `vte` 0.15.0. Its ANSI performer recognizes OSC 9,
  OSC 99, and OSC 777 and emits their untouched semicolon-delimited payload in
  a reserved transient title marker.
- `alacritty_terminal/` is `zed-industries/alacritty` commit
  `fcf32feacb367b75ec84dd40f041e4fd411d3cc1`. It retains a bounded queue of
  unacknowledged markers and emits a versioned replay envelope containing every
  pending payload. Each later envelope is self-contained, so Zed collapsing a
  backend event batch to its final breadcrumb cannot erase an earlier
  notification. The queue is cleared only by an exact watermark ACK written
  back through the terminal; stale ACKs replay the current envelope instead.
  Ordinary process titles remain separate state and are emitted after the
  replay is acknowledged.
- `terminal_view/` is Zed commit
  `abbe85a3321bf6cb7f5b241e623d9c2e16c29187`. It adds one application-owned
  terminal factory hook and uses it for direct split cloning and persisted
  terminal deserialization, closing creation paths that otherwise bypass the
  exact-pane CLI capability setup.

The marker prefix is `U+001F + "zmux-osc-notification-v1:"`. Keep it identical
to `OSC_NOTIFICATION_TITLE_PREFIX` in `src/osc.rs`. The application consumes
the replay from `terminal::Event::BreadcrumbsChanged`, deduplicates its sequence
numbers, processes every payload, and then sends the printable `ack3` OSC 2
watermark. Ordinary title events continue through Zed's normal title path once
the replay queue is empty.

The original VTE and Alacritty licenses and upstream README files are retained
inside their crate directories. The terminal-view snapshot is GPL-3.0-or-later
and is covered by the repository's root `LICENSE`. Local modifications are
limited to the notification bridge/factory hook, their tests, and standalone
Cargo metadata needed for path patches.
