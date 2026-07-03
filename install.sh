#!/bin/sh
# Copyright (c) Inodra
# SPDX-License-Identifier: Apache-2.0
#
# Aquarium installer — downloads a prebuilt `aquarium` binary from GitHub
# Releases and puts it on your PATH. Modeled on Mysten's suiup install.sh.
#
#   curl -fsSL https://raw.githubusercontent.com/inodrahq/aquarium/main/install.sh | sh
#
# Environment overrides:
#   AQUARIUM_VERSION      release tag to install (default: latest)
#   AQUARIUM_INSTALL_DIR  install directory (default: ~/.local/bin)
#   GITHUB_TOKEN          token for the GitHub API (avoids rate limits)

set -eu

REPO="inodrahq/aquarium"
BIN="aquarium"
RELEASES_URL="https://github.com/${REPO}/releases"
API_URL="https://api.github.com/repos/${REPO}/releases"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
info() { printf '%s\n' "$1" >&2; }

need() { command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"; }

detect_os() {
    case "$(uname -s)" in
        Linux*)  echo "Linux" ;;
        Darwin*) echo "macOS" ;;
        *) err "unsupported OS: $(uname -s) (use 'cargo install' from source instead)" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)  echo "x86_64" ;;
        arm64|aarch64) echo "arm64" ;;
        *) err "unsupported architecture: $(uname -m)" ;;
    esac
}

# Latest release tag via the GitHub API (no jq dependency).
latest_version() {
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        resp=$(curl -fsSL -H "Authorization: Bearer ${GITHUB_TOKEN}" "${API_URL}/latest")
    else
        resp=$(curl -fsSL "${API_URL}/latest")
    fi
    tag=$(printf '%s' "$resp" | grep '"tag_name"' | head -1 | cut -d'"' -f4)
    [ -n "$tag" ] || err "could not determine the latest release (set AQUARIUM_VERSION to pin one)"
    echo "$tag"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        err "no sha256 tool found (need sha256sum or shasum)"
    fi
}

choose_install_dir() {
    if [ -n "${AQUARIUM_INSTALL_DIR:-}" ]; then
        echo "$AQUARIUM_INSTALL_DIR"
    elif [ -d "$HOME/.local/bin" ] || mkdir -p "$HOME/.local/bin" 2>/dev/null; then
        echo "$HOME/.local/bin"
    elif [ -w "/usr/local/bin" ]; then
        echo "/usr/local/bin"
    else
        echo "$HOME/bin"
    fi
}

main() {
    need curl
    need tar

    os=$(detect_os)
    arch=$(detect_arch)
    version="${AQUARIUM_VERSION:-$(latest_version)}"
    asset="${BIN}-${os}-${arch}.tar.gz"
    url="${RELEASES_URL}/download/${version}/${asset}"

    info "Installing ${BIN} ${version} (${os}-${arch})"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    info "Downloading ${url}"
    curl -fsSL "$url" -o "$tmp/$asset" || err "download failed: $url"

    # Verify the checksum if the release publishes one (it should).
    if curl -fsSL "${url}.sha256" -o "$tmp/$asset.sha256" 2>/dev/null; then
        want=$(cut -d' ' -f1 < "$tmp/$asset.sha256")
        got=$(sha256_of "$tmp/$asset")
        [ "$want" = "$got" ] || err "checksum mismatch (expected $want, got $got)"
        info "Checksum verified"
    else
        info "warning: no published checksum for $asset; skipping verification"
    fi

    tar -xzf "$tmp/$asset" -C "$tmp" || err "failed to extract $asset"
    [ -f "$tmp/$BIN" ] || err "archive did not contain a '$BIN' binary"

    dir=$(choose_install_dir)
    mkdir -p "$dir"
    install -m 0755 "$tmp/$BIN" "$dir/$BIN" 2>/dev/null || {
        cp "$tmp/$BIN" "$dir/$BIN" && chmod 0755 "$dir/$BIN"
    }
    info "Installed to $dir/$BIN"

    case ":$PATH:" in
        *":$dir:"*) ;;
        *)
            info ""
            info "NOTE: $dir is not on your PATH. Add it, e.g.:"
            info "    echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile"
            ;;
    esac

    info ""
    info "Done. Try:  $BIN info"
}

main "$@"
