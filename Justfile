check:
  @just check-comm
  @just check-daemon
  @just check-esp
  @just check-bindings

build:
  cd daemon && cargo build --release

flash:
  cd espled && cargo flash --release

bindings:
  @just bindings-python
  @just bindings-node

bindings-python:
  @echo "building python bindings"
  cd bindings/python && uv build
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
