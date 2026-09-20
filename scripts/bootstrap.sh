#!/usr/bin/env bash
set -euo pipefail

install_tool() {
  local binary="$1"
  local package="$2"
  local version="$3"
  local cargo_binary="${CARGO_HOME:-$HOME/.cargo}/bin/$binary"
  local user_binary="$HOME/.local/bin/$binary"

  if command -v "$binary" >/dev/null 2>&1; then
    printf '%s is already installed\n' "$binary"
    return
  fi

  if [[ -x "$cargo_binary" ]]; then
    mkdir -p "$HOME/.local/bin"
    ln -sf "$cargo_binary" "$user_binary"
    printf 'linked %s into ~/.local/bin\n' "$binary"
    return
  fi

  cargo install --locked --root "$HOME/.local" --version "$version" "$package"
}

install_tool cargo-nextest cargo-nextest 0.9.145
install_tool cargo-deny cargo-deny 0.20.2
install_tool cargo-machete cargo-machete 0.9.2
# 1.49.0 is the latest release compatible with the pinned Rust toolchain.
install_tool typos typos-cli 1.49.0
install_tool taplo taplo-cli 0.10.0
install_tool cargo-llvm-cov cargo-llvm-cov 0.9.1
install_tool just just 1.58.0
install_tool hyperfine hyperfine 1.20.0
