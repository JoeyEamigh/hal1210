# Repository Guidelines

## Project Structure & Module Organization
- `wayled/` contains the host binary: `src/main.rs` orchestrates Wayland capture (`src/wayland/`), Vulkan compute (`src/gpu/`), and Mystic LED dispatch (`src/led/`); shaders live in `wayled/shaders/` and are compiled via `build.rs`.
- `espled/` is the ESP32 firmware using Embassy + ESP-IDF; entry point is `espled/src/main.rs`, with USB ingestion in `usb.rs` and WS2812 control in `led.rs`.
- `ledcomm/` is a shared `no_std` crate defining the LED frame protocol used by both host and firmware.
- Workspace metadata such as `wayled.code-workspace` supports editor setup; keep host and firmware builds separated to avoid cross-architecture toolchain conflicts.

## Development Commands
- `cargo check` – run in each subcrate to verify type correctness without producing binaries.

YOU MUST RUN ALL COMMANDS IN THE CORRECT SUBCRATE DIRECTORY OTHERWISE THE TOOLCHAINS WILL NOT BE FOUND.

## Coding Style & Naming Conventions
- Target Rust 2024 idioms; modules/files use `snake_case`, types use `CamelCase`, constants stay `UPPER_SNAKE`.
- Instrument long-running tasks with `tracing`; reuse abstractions such as `Compute` and `LedManager` instead of bespoke threads.

## Toolchain & Environment Notes
- Host development requires Linux with a running Wayland compositor and Vulkan-capable GPU; ensure access to `/dev/dri/renderD*`.
- Firmware development expects ESP-IDF v5.x with USB Serial/JTAG access; confirm the device enumerates before flashing.
