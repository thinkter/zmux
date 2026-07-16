# Repository guidance

## Platform dependencies

- Keep `gpui_platform` dependencies target-specific in `Cargo.toml`.
- On macOS, `gpui_platform` must enable the `font-kit` feature so fonts render correctly.
- On Windows, keep `gpui_platform` enabled without `font-kit`.
- On Linux and FreeBSD, keep the existing `font-kit`, `wayland`, and `x11` features.
- Do not combine the macOS and Windows dependency sections unless their distinct feature sets are preserved.
