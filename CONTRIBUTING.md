# Contributing

Thanks for helping with LumaDesk. Bug reports, translation fixes and pull requests are welcome.

## Reporting issues

Please include your Windows version, monitor and DPI setup, the steps to reproduce, and whether Explorer or the taskbar was restarted. Avoid screenshots or logs that show personal file names, application contents or other private data.

## Building

LumaDesk builds on Windows with the Rust version pinned in `rust-toolchain.toml` (MSVC toolchain) and the Windows C++ build tools. Before opening a pull request, run:

```powershell
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

For changes to desktop integration (`app.rs`, `render.rs`, `desktop_icons.rs`), also run `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/smoke.ps1` in an interactive desktop session with LumaDesk closed. It uses an isolated app-data folder and test files it creates itself. Do not restart Explorer or change monitor settings as part of routine testing.

## Design rules

- Keep the native Win32 + Direct2D/DirectWrite architecture. Responsiveness and low idle CPU matter more than features.
- Never do disk or Shell work inside `WM_PAINT`; render targets and GPU resources belong to the GUI thread.
- The Explorer desktop parent can be recreated and its client origin can differ from screen coordinates. Preserve the single-instance mutex, live-window reconciliation, work-area recovery and device-loss handling.
- Groups are references. No code path may move, copy, rename or delete a user's files except through an explicit Shell operation the user asked for (Delete, rename).
- Never silently discard a user's groups or settings. Settings live in `%APPDATA%\LumaDesk`; invalid data must stay recoverable.
- Stay offline and telemetry-free.
- Do not claim performance, DPI or hot-plug behavior that has not been measured. Update the documentation when capabilities change.

## Translations

All interface strings are in `src/i18n.rs`, one row per key with a column per language. A unit test checks that every language has every key and the same `{0}`, `{1}` placeholders. Translations other than Traditional Chinese started as machine translations, so native-speaker fixes are especially welcome.
