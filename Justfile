check:
  @just check-comm
  @just check-daemon
  @just check-esp
  @just check-bindings

build:
  cd daemon && cargo build --release

flash:
  cd espled && cargo flash --release

build-profile:
  cd daemon && CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release

profile-mem:
  @just build-profile
  mkdir -p daemon/prof
  cd daemon && valgrind --tool=massif --massif-out-file=prof/massif.out target/release/hal1210

profile-flame:
  mkdir -p daemon/prof
  cd daemon && CARGO_PROFILE_RELEASE_DEBUG=true cargo flamegraph --output prof/flamegraph.svg

examples:
  @just examples-python
  @just examples-node

examples-python:
  #!/bin/bash
  just bindings-python
  echo "running python examples"
  cd bindings/python
  source ./.venv/bin/activate
  python3 examples/manual_mode.py

examples-node:
  @just bindings-node
  @echo "running nodejs examples"
  cd bindings/node && bun run examples/manual-mode.ts

bindings:
  @just bindings-python
  @just bindings-node

bindings-python:
  @echo "building python bindings"
  cd bindings/python && uv build && uv pip install dist/pyhal1210client-0.1.0.tar.gz
  @echo "python bindings built"

bindings-node:
  @echo "building nodejs bindings"
  cd bindings/node && bun i && bun run build
  @echo "nodejs bindings built"

check-comm:
  @just check-ledcomm
  @just check-shadercomm
  @just check-daemoncomm

check-ledcomm:
  cd ledcomm && cargo check --all-features

check-shadercomm:
  cd shadercomm && cargo check --all-features

check-daemoncomm:
  cd daemoncomm && cargo check --all-features

check-esp:
  cd espled && cargo check

check-daemon:
  cd daemon && cargo check --all-features

check-bindings:
  @just check-bindings-core
  @just check-bindings-python
  @just check-bindings-node

check-bindings-python:
  cd bindings/python && cargo check --all-features

check-bindings-node:
  cd bindings/node && cargo check --all-features

check-bindings-core:
  cd bindings/core && cargo check --all-features

install-systemd:
  @echo "installing systemd service file"
  mkdir -p ~/.config/systemd/user/
  sed "s|<<PWD>>|$(pwd)|g" hal1210.service.in > ~/.config/systemd/user/hal1210.service
  systemctl --user daemon-reload
