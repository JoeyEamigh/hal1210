# Kinect Integration Specification

## Implementation Checklist

- [ ] Stage 0: land `kinect::KinectManager` scaffolding (channels, spawn_blocking init, cancellation plumbing, reconnection loop).
- [ ] Stage 0: add bridge + CLI/daemoncomm stubs
- [ ] Stage 1: expose tilt/LED/flag/status control pathways end-to-end (daemoncomm, CLI, bindings) with serialization-friendly structs.
- [ ] Stage 2: implement gesture/mouse control pipeline and Wayland pointer messaging glue.
- [ ] Stage 3: deliver facial recognition enrollment + layout automation with persisted profiles.
- [ ] Stage 4: wire wakeword/audio capture via OpenWakeWord/ONNX Runtime, emitting events + optional libcec hooks.

## Context & Constraints

- libfreenect headers live under `daemon/src/kinect/freenect/headers` and are `.gitignore`d; they must be inspected manually or via `rg`/terminal tooling because repository-wide search tools cannot index them. The daemon `build.rs` already copies `/usr/include/libfreenect` into that directory (applying local patches) and links against the system-provided `libfreenect.so`, so contributors must install `libfreenect-dev` (or distro equivalent) instead of vendoring binaries.
- The existing `kinect::freenect::Kinect` module is generated from the asynchronous C++ API exposed by `libfreenect.hpp` (callbacks that push frames onto Rust channels). The integration plan assumes we refine and productionize this async-friendly layer rather than backsliding to the synchronous C API.
- The daemon already mixes a `calloop` loop (Wayland) with a Tokio runtime. New Kinect work must follow the existing pattern: fallible initialization with rich tracing, dedicated manager structs, unbounded channels for cross-module messaging, and cancellation via `CancellationToken`.
- Media PC requirements: negligible CPU/GPU impact while idle, bounded allocations (pre-allocated buffers, slab pools) and minimal dynamic allocations, and deterministic teardown when the device is unplugged or absent. Kinect absence should warn but not abort startup.

## High-Level Objectives

- Initialize libfreenect + OpenCV inside the daemon so depth (640×480@30 Hz) and RGB (1280×1024@15 Hz) streams are available simultaneously.
- Surface Kinect control & telemetry (tilt, LEDs, flags, stream status, gesture/facial/audio events) to `daemoncomm`, CLI, and bindings with idiomatic Rust types.
- Provide a foundation for three feature tracks: (1) mouse/gesture control, (2) facial recognition for keyboard layout automation, (3) wakeword/audio capture for voice control.
- Keep the design incremental: each feature builds on the same streaming + message infrastructure so partial delivery is still shippable.

## Near-Term Subplan

1. **Stage 0 Foundations**
  - Mirror `led::LedManager` patterns to define `kinect::{Command, Event, Manager}` plus typed channel aliases and reconnection-aware lifecycle methods.
  - Extend `daemon::main` and `bridge::Handler` with Kinect-aware channel stubs that gracefully handle missing hardware, surfacing `DeviceUnavailable` to clients.
  - Record distro prerequisites (`libfreenect-dev`, CUDA-enabled OpenCV) in docs/CI so contributors immediately see actionable errors.
  - Add `daemon/examples/kinect_status.rs` (or similar) to observe initialization + telemetry while the rest of the pipeline is stubbed.
2. **Stage 1 Device Control APIs**
  - Thread tilt/LED/flag/status commands through daemoncomm, CLI, and bindings, deriving `Serialize/Deserialize` for public structs.
  - Ensure bridge command validation and monitoring counters cover both success/failure paths.
3. **Stage 2 Gesture/Mouse Pipeline**
  - Implement frame buffering, depth/RGB synchronization, and preliminary gesture classification feeding new Wayland pointer commands.
  - Emit monitoring metrics + tracing spans for latency/backpressure while games are detected.
4. **Stages 3–4 (Facial Recognition & Wakeword)**
  - Layer in profile management, DNN inference, and wakeword audio capture atop the established telemetry/command rails without reworking upstream contracts.

## Architecture Overview

- Introduce `kinect::KinectManager` analogous to `led::LedManager`:
  - Owns the freenect context/device, stream configuration, OpenCV pipelines, and OS resources (audio capture).
  - Runs on dedicated threads: one blocking thread for libfreenect USB polling, one or more worker tasks for CPU vision/audio workloads. All external communication is message based.
  - Exposes `CommandTx/CommandRx` and `EventTx/EventRx` types mirroring other subsystems for consistency.
- Extend `daemon::main`:
  1.  Create Kinect command/event channels.
  2.  Attempt `KinectManager::init` inside a `tokio::task::spawn_blocking`. On failure due to missing hardware, log `warn!` and skip spawn while leaving stubs in `bridge::Handler` so clients receive “device unavailable” responses.
  3.  If initialization succeeds, pass channels and a child cancellation token into `bridge::Handler` so it can fan out Kinect events to the rest of the system.
- Augment `bridge::Handler` state machine to:
  - Forward client commands (tilt, LED, flag toggles, query status) to the Kinect manager after validating manual/idle policies.
  - Listen to Kinect events (frame summaries, detected gestures, face IDs, wakewords) and forward them to `wayland`, `client`, or new subsystems.

## Initialization & Lifecycle

1. **Discovery**: `KinectManager::init` enumerates devices via freenect (`freenect_num_devices`, `freenect_list_device_attributes`). Choose the default index unless configuration specifies a serial.
2. **Context Setup**: Manage libfreenect contexts manually because headers are not in the crate root. Use the existing autocxx bindings (`Freenect::Freenect`) to configure:
   - Depth mode: `FREENECT_DEPTH_REGISTERED` or `FREENECT_DEPTH_MM` @ `FREENECT_RESOLUTION_MEDIUM` (640×480) with 30 Hz.
   - RGB mode: `FREENECT_VIDEO_RGB` @ `FREENECT_RESOLUTION_HIGH` (1280×1024) with 15 Hz.
   - Simultaneous streams require distinct buffers; pre-allocate aligned arenas sized via `getDepthBufferSize`/`getVideoBufferSize` and reuse them using `bytes::BytesMut` or custom pool structs to avoid per-frame Vec allocations.
3. **Threading**:
   - `DeviceThread`: blocking loop that repeatedly calls `freenect_process_events_timeout`. It writes frame metadata into lock-free ring buffers and signals worker tasks via MPSC channels.
   - `ProcessingTasks`: Tokio tasks subscribe to the rings, convert raw buffers into `opencv::core::Mat` via `Mat::new_rows_cols_with_data` (wrapping existing memory), and run detection algorithms.
   - `AudioCaptureThread`: optional until Stage 3 (wakeword) but plan the scaffolding so ALSA/Pulse/PortAudio capture can be attached later without redesigning the manager.
4. **Graceful Degradation**: if the Kinect disconnects mid-run, report a `KinectEvent::DeviceLost` so `bridge` can notify clients and keep the daemon alive. Attempt reconnection with exponential backoff while honoring cancellation tokens.

## Command & Event Channels

- `kinect::Command` (sent by bridge/clients):
  - `StartStreams`, `StopStreams`, `Resync` (reset detection band), `Set_tilt { degrees }`, `Set_led { state }`, `Set_flags { bitmask/enum }`, `RequestStatus`, `CalibrateHandZone`, `SetUserProfile(UserId)`, `EnableMouse(bool)`, `EnableFacialRecognition(bool)`, `EnableWakeword(bool)`.
- `kinect::Event` (emitted by manager):
  - `Status { connected, fw_versions, temperatures, tilt_state, flags, stream_config, fps_estimates }`.
  - `FrameStats { queue_depth, dropped }` for monitoring.
  - `HandGesture(HandGestureEvent)` including gesture type, tracking id, 2D cursor delta, confidence, depth band info, timestamp.
  - `FaceEvent(FaceObservation)` carrying identity, confidence, “keyboard possession” score, and suggested input layout.
  - `WakewordEvent { keyword, confidence }` once audio capture is active.
  - `DeviceLost`/`DeviceRecovered` to drive UI feedback and retries.
- Bridge should translate events into:
  1.  Wayland commands for pointer control (`wayland::Command::MouseInput { motion, buttons }`—new enum variants to be added).
      - Actual pointer injection will be performed through a future `wayland::Command::VirtualPointer(z w l r_virtual_pointer_v1::Request)` helper so that we can exclusively hijack the cursor via `zwlr_virtual_pointer_manager_v1` when the Kinect is in charge.
  2.  Client notifications via `daemoncomm::MessageToClientData::Kinect(KinectNotification)`.

## Streaming & Buffering Strategy

- **Depth path**:
  - Format: 16-bit depth in millimeters. Maintain two ring buffers: raw depth frames and temporally averaged frames to smooth noise without allocation (use fixed-size arrays of `640*480` `u16` inside `Arc<AtomicU16>` or `Box<[u16; 307200]>` reused via `VecDeque`).
  - Convert into OpenCV Mats with type `CV_16UC1`. Additional downsampled views (e.g., `CV_8UC1`) are derived in-place via `cv::convertScaleAbs` with scratch buffers from a pool.
- **RGB path**:
  - Format: 8-bit `RGB24`. Keep triple-buffered `Vec<u8>` assigned per frame to avoid clobber while processing.
  - Downscale to 640×480 for CPU-bound detectors; store both high-res (for face embeddings) and low-res (for quick gesture classification).
- **Synchronization**: match RGB + depth frames using timestamps. Maintain `BTreeMap` keyed by `timestamp` to align frames for combined algorithms, with pruning to keep ≤250 ms of history.
- **Backpressure**: if processing lags, drop the oldest frames rather than blocking libfreenect to keep latency acceptable. Emit counters to monitoring to surface when this occurs.

## Game Activity Detection (Wayland + Hyprland)

- Subscribe to `zwlr_foreign_toplevel_manager_v1` to receive lifecycle events for all windows, recording app-id, title, fullscreen/maximized state, and PID (via `xdg_toplevel` uids). Mark “game active” when the focused surface is fullscreen, on the `gaming` workspace, or matches a curated allowlist (Steam, Lutris, Heroic, RetroArch, etc.).
- Augment this with Hyprland’s IPC socket (`/tmp/hypr/<instance>/.socket2.sock`) to read `activewindow` / `workspace` JSON events for richer metadata (e.g., whether the surface is XWayland, floating, or has `fullscreen` toggled). The combination lets us handle compositor-specific hints without waiting for generic Wayland protocols.
- Feed the resulting state into `bridge::Handler` so Kinect-driven features can lower their sampling rate (or stop pointer hijack) whenever a game is in focus, minimizing contention with high-performance workloads.

## Client-Facing API Updates (daemoncomm + bindings)

- Extend `daemoncomm::MessageToServerData` with `Kinect(KinectCommand)` and `MessageToClientData` with `Kinect(KinectNotification)`.
- Define Rust-native structs for:
  - `KinectStatus`: connection, tilt angle, accel readings converted into m/s², LED state, firmware info, current detection mode, active users.
  - `KinectFlags`: strongly typed bitflags mirroring `freenect_flag` but ergonomic (e.g., `auto_exposure`, `mirror_depth`).
  - `GestureDescriptor`, `FaceIdentity`, `WakewordHit`.
- CLI/Bidi bindings: add subcommands (`kinect status`, `kinect tilt 10`, `kinect led green`, `kinect mouse enable`) and produce JSON/Bincode responses consistent with new structs.
- All payloads must be serialization-friendly (derive `Serialize/Deserialize`) and avoid leaking raw FFI pointers.

## Feature Implementation Roadmap

1. **Stage 0 – Foundations**

   - Keep relying on distro-provided `libfreenect` + headers (already handled by `daemon/build.rs`). Document the `libfreenect-dev` (or matching package) prerequisite in README/Justfile and ensure CI bails out with a helpful message if the libs are missing.
   - Implement `KinectManager` scaffolding: initialization, stream start/stop, status reporting, command/event plumbing, reconnection loop.
   - Add monitoring metrics (frame rate, queue depth, CPU usage) via `tracing` and potential `metrics` crate.

2. **Stage 1 – Device Control APIs**

   - Wire `daemoncomm`/CLI/bindings to control tilt, LED, flags, and to fetch status—ensuring absence is nonfatal (responses indicate `DeviceUnavailable`).
   - Provide a structured telemetry event (`KinectEvent::Status`) for UI/logging.

3. **Stage 2 – Mouse Control Pipeline**

   - **Detection Band Logic**: when RGB gestures detect an open hand extended beyond the calibrated couch plane, snapshot the concurrent depth slice (e.g., mean depth ±3 cm) and store as the active band.
   - **Hand Tracking**:
     - Use depth segmentation (threshold by band, morphological cleanup, contour extraction) to isolate hands. Use RGB skin-color + motion cues to confirm identity, reducing false positives.
     - Assign persistent IDs via Hungarian matching on centroid + depth velocity to “ensure the same hand” is tracked.
   - **Gesture Classification**: CNN/SVM or OpenCV template to distinguish open vs. closed fist. Keep modular so new gestures plug into the classifier trait.
   - **Wayland Messaging**: define `wayland::Command::PointerDelta { dx, dy }` and `wayland::Command::PointerButton { button, state }`. Bridge converts `HandGestureEvent` into these commands and also into CLI notifications so other consumers can react.
   - **Timeout Handling**: if no matching hand is seen for >10 s or confidence drops, emit `HandGestureEvent::Lost` to disable mouse control gracefully.

4. **Stage 3 – Facial Recognition & Keyboard Layout Automation**

   - Capture 1280×1024 frames at 15 Hz; downscale to 320×256 for face detection, but keep high-res crops for embedding via OpenCV’s DNN (e.g., `Facenet`, `OpenFace`, or ONNX runtime if available).
   - Maintain user profiles (embedding vectors + metadata) inside `~/.config/hal1210/faces.json` (or similar flat-file store). Provide CLI tooling to enroll faces with associated keyboard layouts (DVORAK, QWERTY, etc.) and to revoke/update entries.
   - Detect when a recognized face is aligned with the keyboard (heuristics: bounding box overlapping keyboard ROI, optional colorful markers). If uncertain, allow fallback gesture (e.g., “switch layout” gesture) that references the recognized identity.
   - Emit `KinectEvent::ActiveUser { user_id, layout }`, and have bridge call out to a small helper (systemd DBus or `kanata` management) to toggle the correct layout service. Keep pipeline optional so gaming sessions can disable it.

5. **Stage 4 – Wakeword & Voice Hooks**
   - Activate microphone capture via libfreenect audio APIs. Provide a ring buffer feeding a wakeword detector (OpenWakeWord, Silero, or custom MFCC + lightweight classifier). Continue to enforce minimal CPU usage by downsampling to 16 kHz and batching inference.
   - Standardize on the open-source [OpenWakeWord](https://github.com/dscripka/openWakeWord) engine (MIT-licensed, no API keys). Install via `pip install openwakeword`, download the pre-trained `.onnx` keyword models into `~/.config/hal1210/openwakeword/`, and allow custom models to be dropped alongside for future gestures/phrases. Runtime-wise we will embed ONNX Runtime (`ort` crate) directly in Rust, re-implementing the Python reference preprocessing pipeline (16 kHz mono PCM → log-mel features) so no Python interpreter needs to stay resident; Python is only required to fetch/update models.
   - On wakeword hit, emit `KinectEvent::Wakeword { keyword, confidence }` plus a short audio snippet for context. Later stages can forward the snippet to an AI backend; for now, plan for message types that carry audio handles (e.g., path to tmpfile, shared memory descriptor) rather than blobs in RAM), and also schedule downstream actions such as toggling the TV via libcec when “Hey HAL” is recognized.

## Performance & Reliability Considerations

- **Memory reuse**: implement custom buffer pools for depth/RGB/audio frames. Avoid `Vec::to_vec()` in callbacks—switch to writing into pre-allocated `Box<[u8]>` slices passed to libfreenect via `setDepthBuffer`/`setVideoBuffer` once that plumbing is available.
- **CPU pinning**: run heavy OpenCV stages on dedicated worker threads with lower priority (`linux` `sched_setaffinity`/`nice`) so gaming workloads remain unaffected.
- **Tracing**: instrument all major stages (frame receipt, processing start/finish, classifier decisions) with `tracing::trace!` guards to aid future tuning. Emit `tracing::warn!` on dropped frames or inference overruns.
- **Power**: pause RGB/depth processing (while keeping USB alive) whenever system idle + no client interest, resuming on demand to keep idle power low.
- **GPU acceleration**: build OpenCV with CUDA enabled (preferred on this NVIDIA-only host) while keeping optional Vulkan/Umat fallbacks for systems without proprietary drivers. Keep the wakeword pipeline CPU-bound (OpenWakeWord) but allow future CUDA/Vulkan kernels for heavier audio models if we adopt larger transformer-based speech modules.
- **Power**: pause RGB/depth processing (while keeping USB alive) whenever system idle + no client interest, resuming on demand to keep idle power low.
- **Safety**: wrap unsafe freenect calls carefully (already in `kinect::freenect`); ensure `Drop` stops streams before buffers are freed.

## Testing & Validation Strategy

- **Unit tests**: synthetic frame generators feeding hand/facial detectors to validate math without hardware.
- **Integration tests**: cargo feature-gated tests that run only when the cargo feature is active, initializing the manager and verifying channel interactions.
- **Monitoring harness**: extend `daemon/examples` with a `kinect_status.rs` to print telemetry during manual QA.
- **Simulation**: allow replaying recorded RGB/depth streams from disk to exercise gesture/face pipelines offline (use `.oni` or custom raw dumps).

## Open Questions / Follow-Ups

- Clarify how Wayland pointer injection should behave when other pointing devices are active (exclusive mode vs. cooperative blending, escape gesture, etc.).
- Define retention/encryption policies for `~/.config/hal1210/faces.json` (e.g., optional passphrase or OS keyring integration) even though the storage format is agreed on.
- Decide when Kinect-triggered wakewords should also emit libcec commands (e.g., avoid powering on the TV if the AVR already reports `on`).
