#!/bin/sh
# Copyright (c) Inodra
# SPDX-License-Identifier: Apache-2.0
#
# Aquarium installer — a small wizard that asks which Sui channel you want
# (testnet is the default) and whether to install a prebuilt binary from
# GitHub Releases or build from source, then does it.
#
#   curl -fsSL https://raw.githubusercontent.com/inodrahq/aquarium/main/install.sh | sh
#
# Non-interactive (CI, scripts) — set the answers up front; anything unset
# falls back to its default without prompting when no terminal is available:
#   AQUARIUM_CHANNEL      mainnet | testnet | devnet   (default: testnet)
#   AQUARIUM_METHOD       prebuilt | source            (default: prebuilt)
#   AQUARIUM_VERSION      release tag for prebuilt (default: latest)
#   AQUARIUM_INSTALL_DIR  install directory (default: ~/.local/bin)
#   GITHUB_TOKEN          token for the GitHub API (avoids rate limits)
#
# Channels: testnet tracks Mysten's testnet cut (same protocol support as
# mainnet ~a week early — never trails a protocol activation), mainnet is
# exact validator parity, devnet is bleeding edge (source builds only).

set -eu

REPO="inodrahq/aquarium"
BIN="aquarium"
RELEASES_URL="https://github.com/${REPO}/releases"
API_URL="https://api.github.com/repos/${REPO}/releases"
DEFAULT_CHANNEL="testnet"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
info() { printf '%s\n' "$1" >&2; }

need() { command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"; }

# Prompt on the controlling terminal so the wizard works under `curl | sh`
# (stdin is the script). Falls back to the default when there is no tty.
ask() { # ask <prompt> <default> -> stdout
    prompt=$1 default=$2
    # The device node existing is not enough — actually try to open the
    # controlling terminal (there is none under CI or `< /dev/null`).
    if (: < /dev/tty) 2>/dev/null && (: > /dev/tty) 2>/dev/null; then
        printf '%s [%s]: ' "$prompt" "$default" > /dev/tty
        IFS= read -r answer < /dev/tty || answer=""
        printf '%s' "${answer:-$default}"
    else
        printf '%s' "$default"
    fi
}

detect_os() {
    case "$(uname -s)" in
        Linux*)  echo "Linux" ;;
        Darwin*) echo "macOS" ;;
        *) err "unsupported OS: $(uname -s) (build from source with AQUARIUM_METHOD=source)" ;;
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

install_binary() { # install_binary <path>
    dir=$(choose_install_dir)
    mkdir -p "$dir"
    install -m 0755 "$1" "$dir/$BIN" 2>/dev/null || {
        cp "$1" "$dir/$BIN" && chmod 0755 "$dir/$BIN"
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

install_prebuilt() { # install_prebuilt <channel>
    need tar
    channel=$1
    os=$(detect_os)
    arch=$(detect_arch)
    version="${AQUARIUM_VERSION:-$(latest_version)}"
    asset="${BIN}-${channel}-${os}-${arch}.tar.gz"
    url="${RELEASES_URL}/download/${version}/${asset}"

    info "Installing ${BIN} ${version} (${channel}, ${os}-${arch})"

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

    install_binary "$tmp/$BIN"
}

install_from_source() { # install_from_source <channel>
    need git
    command -v cargo >/dev/null 2>&1 \
        || err "cargo not found — install Rust first: https://rustup.rs"
    channel=$1

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    info "Cloning ${REPO}"
    git clone --quiet --depth 1 "https://github.com/${REPO}" "$tmp/src"

    # Re-pin the Sui crates to the chosen channel's tag from channels.toml.
    # The repo pins the default channel; other channels are a build-time swap.
    repo_tag=$(grep -om1 '[a-z]*net-v[0-9.]*' "$tmp/src/Cargo.toml")
    channel_tag=$(grep "^${channel} " "$tmp/src/channels.toml" | cut -d'"' -f2)
    [ -n "$channel_tag" ] || err "channel '$channel' not found in channels.toml"
    if [ "$repo_tag" != "$channel_tag" ]; then
        info "Re-pinning Sui crates: ${repo_tag} -> ${channel_tag}"
        sed "s/${repo_tag}/${channel_tag}/g" "$tmp/src/Cargo.toml" > "$tmp/src/Cargo.toml.new"
        mv "$tmp/src/Cargo.toml.new" "$tmp/src/Cargo.toml"
        (cd "$tmp/src" && cargo update --quiet sui-types sui-data-store sui-execution)
    fi

    info "Building ${BIN} (${channel_tag}) — the Sui execution tree takes a few minutes"
    (cd "$tmp/src" && cargo build --release)

    install_binary "$tmp/src/target/release/$BIN"
}

main() {
    need curl

    channel="${AQUARIUM_CHANNEL:-}"
    method="${AQUARIUM_METHOD:-}"

    if [ -z "$channel" ]; then
        info "Which Sui channel? testnet never trails a mainnet protocol"
        info "activation; mainnet is exact validator parity; devnet is bleeding edge."
        channel=$(ask "Channel (mainnet/testnet/devnet)" "$DEFAULT_CHANNEL")
    fi
    case "$channel" in
        mainnet|testnet|devnet) ;;
        *) err "unknown channel: $channel (expected mainnet, testnet or devnet)" ;;
    esac

    if [ "$channel" = "devnet" ] && [ "${method:-prebuilt}" != "source" ]; then
        info "devnet has no prebuilt binaries (it moves too fast) — building from source."
        method="source"
    fi
    if [ -z "$method" ]; then
        method=$(ask "Install prebuilt binary or build locally? (prebuilt/source)" "prebuilt")
    fi

    case "$method" in
        prebuilt) install_prebuilt "$channel" ;;
        source)   install_from_source "$channel" ;;
        *) err "unknown method: $method (expected prebuilt or source)" ;;
    esac
}

main "$@"
