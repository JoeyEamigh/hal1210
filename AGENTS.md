# Repository Guidelines

## Project Structure & Module Organization
- `daemon/` contains the host binary: `src/main.rs` orchestrates Wayland capture (`src/wayland/`), Vulkan compute (`src/gpu/`), and ESP32 LED command dispatch (`src/led/`); shaders live in `wayled/shaders/` and are compiled via `build.rs`.
- `espled/` is the ESP32 firmware using esp-hal nostd; entry point is `espled/src/bin/main.rs`.

## Development Commands
- `just check` – run all checks across all crates. this is the main command you should use when developing.
- `cargo check` – run in a crate directory to verify type correctness without producing binaries.

YOU MUST RUN ALL CARGO COMMANDS IN THE CORRECT CRATE DIRECTORY OTHERWISE THE TOOLCHAINS WILL NOT BE FOUND.

## Coding Style & Naming Conventions
- Target Rust 2024 idioms; modules/files use `snake_case`, types use `CamelCase`, constants stay `UPPER_SNAKE`. Keep imports from the same module combined in one `use` statement.
- Instrument long-running tasks with fully qualified `tracing`, use trace! and debug! logs for debugging info. trace with string interop instead of adding tags to spans.
- Prefer `?` for error propagation; use `thiserror` for custom error types.
- !! DO NOT WRITE COMMENTS unless there is NO WAY to make the code self-explanatory. !!

## Important Tips
Your search tools are not capable of searching in gitignored files. If you need to search in such files, use a terminal command like `rg`.
