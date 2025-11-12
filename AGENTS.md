# Repository Guidelines

## Project Structure & Module Organization
- `wayled/` contains the host binary: `src/main.rs` orchestrates Wayland capture (`src/wayland/`), Vulkan compute (`src/gpu/`), and ESP32 LED command dispatch (`src/led/`); shaders live in `wayled/shaders/` and are compiled via `build.rs`.
- `espled/` is the ESP32 firmware using esp-hal nostd; entry point is `espled/src/bin/main.rs`.
- `ledcomm/` is a shared `no_std` crate defining the LED frame protocol used by both host and firmware.

## Development Commands
- `cargo fmt` – run in each subcrate to format code according to Rust style guidelines.
- `cargo check` – run in each subcrate to verify type correctness without producing binaries.

YOU MUST RUN ALL COMMANDS IN THE CORRECT SUBCRATE DIRECTORY OTHERWISE THE TOOLCHAINS WILL NOT BE FOUND.

## Coding Style & Naming Conventions
- Target Rust 2024 idioms; modules/files use `snake_case`, types use `CamelCase`, constants stay `UPPER_SNAKE`.
- Instrument long-running tasks with `tracing`, use trace! and debug! logs for debugging info. trace with string interop instead of adding tags to spans.
