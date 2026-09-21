#!/usr/bin/env bash
# Install diffscope for the current platform from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/ieVictor/diffscope/master/scripts/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/ieVictor/diffscope/master/scripts/install.sh | bash -s -- --no-setup
#
# The archive is downloaded from https://github.com/ieVictor/diffscope/releases,
# checked against the published SHA-256 digest, and only then extracted and
# installed under the per-user install directory. On success the installer runs
# "diffscope setup" unless --no-setup is given.
set -euo pipefail

readonly repository="ieVictor/diffscope"
readonly latest_release_url="https://api.github.com/repos/${repository}/releases/latest"
readonly release_download_url="https://github.com/${repository}/releases/download"

version=""
install_dir="${HOME:+${HOME}/.local/bin}"
configure=true

usage() {
  cat <<'EOF'
Install diffscope from GitHub Releases for the current platform.

Usage: install.sh [options]

Options:
  --version <tag>      Release tag to install, for example v0.2.0 (a leading
                       "v" is optional). Defaults to the latest release.
  --install-dir <dir>  Absolute directory that receives the diffscope binary.
                       Defaults to $HOME/.local/bin.
  --no-setup           Install the binary without running "diffscope setup".
  --help               Print this message.

Supported platforms: x86_64/aarch64 Linux (static musl), x86_64/aarch64 macOS.
On Windows use scripts/install.ps1 instead.
EOF
}

require_value() {
  if (($# < 2)); then
    printf 'diffscope installer: %s requires a value\n' "$1" >&2
    exit 2
  fi
}

while (($# > 0)); do
  case "$1" in
    --version)
      require_value "$@"
      version="$2"
      shift 2
      ;;
    --install-dir)
      require_value "$@"
      install_dir="$2"
      shift 2
      ;;
    --no-setup)
      configure=false
      shift
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    *)
      printf 'diffscope installer: unknown option %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

for tool in curl tar; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf 'diffscope installer: %s is required but was not found on PATH\n' "$tool" >&2
    exit 1
  fi
done

if [[ -z $install_dir ]]; then
  printf 'diffscope installer: --install-dir is required when HOME is not set\n' >&2
  exit 2
fi

if [[ $install_dir != /* ]]; then
  printf 'diffscope installer: --install-dir must be an absolute path, got %s\n' "$install_dir" >&2
  exit 2
fi

# Map the host to one of the released target triples, or fail: an unsupported
# platform never falls back to a binary built for a different target. Linux
# installs the statically linked musl build, which runs on any distribution.
platform_target() {
  local os arch os_suffix arch_prefix
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux) os_suffix="unknown-linux-musl" ;;
    Darwin) os_suffix="apple-darwin" ;;
    *) return 1 ;;
  esac

  case "$arch" in
    x86_64 | amd64) arch_prefix="x86_64" ;;
    aarch64 | arm64) arch_prefix="aarch64" ;;
    *) return 1 ;;
  esac

  printf '%s-%s\n' "$arch_prefix" "$os_suffix"
}

if ! target="$(platform_target)"; then
  printf 'diffscope installer: unsupported platform %s %s\n' "$(uname -s)" "$(uname -m)" >&2
  printf 'released targets: x86_64/aarch64 Linux (static musl), x86_64/aarch64 macOS, x86_64 Windows\n' >&2
  exit 1
fi

download() {
  local url="$1" destination="$2"
  if ! curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' \
    --output "$destination" "$url"; then
    printf 'diffscope installer: could not download %s\n' "$url" >&2
    exit 1
  fi
}

resolve_latest_tag() {
  local response
  if ! response="$(curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' "$latest_release_url")"; then
    printf 'diffscope installer: could not query %s\n' "$latest_release_url" >&2
    printf 'pass --version <tag> to install a specific release\n' >&2
    exit 1
  fi

  local tag
  tag="$(printf '%s\n' "$response" | awk -F'"' '$2 == "tag_name" { print $4 }')"
  if [[ -z $tag ]]; then
    printf 'diffscope installer: %s did not report a release tag\n' "$latest_release_url" >&2
    printf 'pass --version <tag> to install a specific release\n' >&2
    exit 1
  fi

  printf '%s\n' "$tag"
}

if [[ -n $version ]]; then
  tag="v${version#v}"
  if [[ ! $tag =~ ^v[0-9] ]]; then
    printf 'diffscope installer: invalid version %s; expected a release tag such as v0.2.0\n' "$version" >&2
    exit 2
  fi
else
  tag="$(resolve_latest_tag)"
fi

asset="diffscope-${tag}-${target}.tar.gz"
release_url="${release_download_url}/${tag}"

work="$(mktemp -d "${TMPDIR:-/tmp}/diffscope-install.XXXXXX")"
trap 'rm -rf "$work"' EXIT

printf 'downloading %s\n' "$asset"
download "${release_url}/${asset}" "${work}/${asset}"
download "${release_url}/SHA256SUMS" "${work}/SHA256SUMS"

expected_digest="$(awk -v asset="$asset" '
  {
    name = $2
    sub(/^\*/, "", name)
    if (name == asset) {
      matches += 1
      digest = $1
    }
  }
  END { if (matches == 1) print digest }
' "${work}/SHA256SUMS")"

if [[ -z $expected_digest ]]; then
  printf 'diffscope installer: %s is not listed exactly once in SHA256SUMS; refusing to install\n' "$asset" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  actual_digest="$(sha256sum "${work}/${asset}" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
  actual_digest="$(shasum -a 256 "${work}/${asset}" | awk '{ print $1 }')"
elif command -v openssl >/dev/null 2>&1; then
  actual_digest="$(openssl dgst -sha256 "${work}/${asset}" | awk '{ print $NF }')"
else
  printf 'diffscope installer: no SHA-256 tool found (need sha256sum, shasum, or openssl)\n' >&2
  exit 1
fi

expected_digest="$(printf '%s' "$expected_digest" | tr '[:upper:]' '[:lower:]')"
actual_digest="$(printf '%s' "$actual_digest" | tr '[:upper:]' '[:lower:]')"
if [[ $actual_digest != "$expected_digest" ]]; then
  printf 'diffscope installer: checksum mismatch for %s; refusing to install\n' "$asset" >&2
  printf '  expected %s\n' "$expected_digest" >&2
  printf '  actual   %s\n' "$actual_digest" >&2
  exit 1
fi

mkdir -p "${work}/unpacked"
if ! tar -xzf "${work}/${asset}" -C "${work}/unpacked"; then
  printf 'diffscope installer: could not extract %s\n' "$asset" >&2
  exit 1
fi

# The release archives hold the binary at the archive root; anything else means
# the download and this installer disagree, so stop instead of guessing.
source_binary="${work}/unpacked/diffscope"
if [[ ! -f $source_binary ]]; then
  printf 'diffscope installer: %s does not contain diffscope at the archive root\n' "$asset" >&2
  exit 1
fi

if [[ ! -d $install_dir ]] && ! mkdir -p "$install_dir"; then
  printf 'diffscope installer: could not create %s\n' "$install_dir" >&2
  exit 1
fi

destination="${install_dir}/diffscope"
staged="${install_dir}/.diffscope.$$"
if ! cp "$source_binary" "$staged" || ! chmod 755 "$staged" || ! mv -f "$staged" "$destination"; then
  rm -f "$staged"
  printf 'diffscope installer: could not install %s into %s\n' "$asset" "$install_dir" >&2
  exit 1
fi

if ! reported_version="$("$destination" --version)"; then
  printf 'diffscope installer: %s failed to run after installation\n' "$destination" >&2
  exit 1
fi
printf 'installed %s (%s)\n' "$destination" "$reported_version"

case ":${PATH:-}:" in
  *":${install_dir}:"*) ;;
  *) printf 'note: %s is not on PATH; add it to your shell profile to call diffscope directly\n' "$install_dir" ;;
esac

if [[ $configure == true ]]; then
  printf 'configuring coding harnesses with %s setup\n' "$destination"
  if ! "$destination" setup; then
    printf 'diffscope installer: %s is installed, but "diffscope setup" failed\n' "$destination" >&2
    printf 'rerun "%s setup" to retry\n' "$destination" >&2
    exit 1
  fi
else
  printf 'skipped harness configuration (--no-setup); run "%s setup" when ready\n' "$destination"
fi
