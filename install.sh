#!/bin/sh
# agentmail installer for Linux and WSL.
#
#   curl -fsSL https://raw.githubusercontent.com/quinnjr/agentmail/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/quinnjr/agentmail/main/install.sh | sh -s -- --harness auto
#
# Downloads the prebuilt static binary for this machine from the GitHub
# release, checks its SHA-256, and installs it into ~/.local/bin. Builds from
# source with cargo when there is no prebuilt binary (or with --from-source).
# Run with --help for the options.

set -eu

REPO="quinnjr/agentmail"

usage() {
    cat <<'EOF'
agentmail installer

Usage: install.sh [options]
  (piped: curl -fsSL .../install.sh | sh -s -- [options])

Options:
  --version <tag>      Release to install, e.g. v0.1.0 (default: latest)
                       [env AGENTMAIL_VERSION]
  --bin-dir <dir>      Where to put the binary (default: ~/.local/bin)
                       [env AGENTMAIL_INSTALL_DIR]
  --harness <name>     Afterwards run `agentmail install --harness <name>`:
                       claude-code, opencode or kiro. Repeatable. `auto`
                       picks each harness whose CLI is on PATH.
  --service            Afterwards run `agentmail install --service` (systemd)
  --from-source        Build with `cargo install` instead of downloading
  -h, --help           Show this help

Mirrors and tests can point downloads elsewhere with AGENTMAIL_DOWNLOAD_URL
(default: https://github.com/quinnjr/agentmail/releases).
EOF
}

say() { printf 'agentmail: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }
has() { command -v "$1" >/dev/null 2>&1; }

# download <url> <file>: fails on HTTP errors.
download() {
    if has curl; then
        curl --proto '=https,file' --tlsv1.2 -fsSL "$1" -o "$2"
    elif has wget; then
        wget -q -O "$2" "$1"
    else
        die "need curl or wget to download"
    fi
}

sha256_of() {
    if has sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif has shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "need sha256sum or shasum to verify the download"
    fi
}

# The Rust target triple of the prebuilt binary for this machine, or nothing
# when there is none.
target_triple() {
    case "$(uname -m)" in
        x86_64 | amd64) echo "x86_64-unknown-linux-musl" ;;
        aarch64 | arm64) echo "aarch64-unknown-linux-musl" ;;
        *) ;;
    esac
}

check_platform() {
    case "$(uname -s)" in
        Linux) ;;
        Darwin) die "macOS is not supported yet (agentmail needs Linux or WSL)" ;;
        MINGW* | MSYS* | CYGWIN*)
            die "on Windows, run install.ps1 or install.cmd: agentmail runs inside WSL" ;;
        *) die "unsupported OS $(uname -s): agentmail needs Linux or WSL" ;;
    esac
}

# fetch_prebuilt <tmpdir> <triple>: downloads, verifies and unpacks the
# release archive into <tmpdir>/unpacked. Fails if any step fails.
fetch_prebuilt() {
    base="${AGENTMAIL_DOWNLOAD_URL:-https://github.com/$REPO/releases}"
    archive="agentmail-$2.tar.gz"
    if [ "$version" = latest ]; then
        url="$base/latest/download/$archive"
    else
        url="$base/download/$version/$archive"
    fi
    say "downloading $url"
    download "$url" "$1/$archive" || return 1
    download "$url.sha256" "$1/$archive.sha256" ||
        die "the release has no checksum for $archive; refusing to install it unverified"
    want="$(cut -d' ' -f1 <"$1/$archive.sha256")"
    got="$(sha256_of "$1/$archive")"
    [ "$want" = "$got" ] || die "checksum mismatch for $archive (expected $want, got $got)"
    mkdir "$1/unpacked"
    tar -xzf "$1/$archive" -C "$1/unpacked"
    [ -f "$1/unpacked/agentmail" ] || die "$archive has no agentmail binary"
}

# build_from_source <tmpdir>: cargo install into <tmpdir>/root.
build_from_source() {
    has cargo || die "no prebuilt binary to use, and cargo is not installed.
Install Rust from https://rustup.rs and run this again, or use a release from
https://github.com/$REPO/releases"
    say "building from source with cargo (this takes a few minutes)"
    if [ "$version" = latest ]; then
        cargo install --locked --git "https://github.com/$REPO" --root "$1/root" agentmail
    else
        cargo install --locked --git "https://github.com/$REPO" --tag "$version" --root "$1/root" agentmail
    fi
    mkdir "$1/unpacked"
    cp "$1/root/bin/agentmail" "$1/unpacked/agentmail"
}

main() {
    version="${AGENTMAIL_VERSION:-latest}"
    bin_dir="${AGENTMAIL_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}"
    harnesses=""
    service=0
    from_source=0

    while [ $# -gt 0 ]; do
        case "$1" in
            --version) [ $# -ge 2 ] || die "--version needs a value"; version="$2"; shift 2 ;;
            --version=*) version="${1#*=}"; shift ;;
            --bin-dir) [ $# -ge 2 ] || die "--bin-dir needs a value"; bin_dir="$2"; shift 2 ;;
            --bin-dir=*) bin_dir="${1#*=}"; shift ;;
            --harness) [ $# -ge 2 ] || die "--harness needs a value"; harnesses="$harnesses $2"; shift 2 ;;
            --harness=*) harnesses="$harnesses ${1#*=}"; shift ;;
            --service) service=1; shift ;;
            --from-source) from_source=1; shift ;;
            -h | --help) usage; exit 0 ;;
            *) usage >&2; die "unknown option: $1" ;;
        esac
    done

    case "$version" in
        latest | v*) ;;
        *) version="v$version" ;;
    esac
    for h in $harnesses; do
        case "$h" in
            claude-code | opencode | kiro | auto) ;;
            *) die "unknown harness '$h' (expected claude-code, opencode, kiro or auto)" ;;
        esac
    done

    check_platform

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT INT TERM

    triple="$(target_triple)"
    if [ "$from_source" = 1 ]; then
        build_from_source "$tmp"
    elif [ -z "$triple" ]; then
        say "no prebuilt binary for $(uname -m)"
        build_from_source "$tmp"
    elif ! fetch_prebuilt "$tmp" "$triple"; then
        say "could not download a prebuilt binary for $triple"
        build_from_source "$tmp"
    fi

    mkdir -p "$bin_dir"
    bin="$bin_dir/agentmail"
    upgrading=0
    [ -e "$bin" ] && upgrading=1
    # Copy next to the target and rename over it, so a running daemon keeps
    # its old binary and nothing ever sees a half-written file.
    cp "$tmp/unpacked/agentmail" "$bin.new"
    chmod 755 "$bin.new"
    mv -f "$bin.new" "$bin"
    say "installed $("$bin" --version) to $bin"

    # A daemon of the old binary may still be running; stop it so the next
    # command starts the new one (under systemd, the unit restarts it).
    if [ "$upgrading" = 1 ] && "$bin" daemon status >/dev/null 2>&1; then
        "$bin" daemon stop >/dev/null 2>&1 && say "stopped the running daemon so the new version takes over"
    fi

    for h in $harnesses; do
        if [ "$h" = auto ]; then
            found=0
            has claude && { "$bin" install --harness claude-code; found=1; }
            has opencode && { "$bin" install --harness opencode; found=1; }
            has kiro-cli && { "$bin" install --harness kiro; found=1; }
            [ "$found" = 1 ] || say "--harness auto: no claude, opencode or kiro-cli on PATH"
        else
            "$bin" install --harness "$h"
        fi
    done
    [ "$service" = 1 ] && "$bin" install --service

    case ":$PATH:" in
        *":$bin_dir:"*) ;;
        *)
            say "$bin_dir is not on your PATH; add this to your shell profile:"
            printf '\n    export PATH="%s:$PATH"\n\n' "$bin_dir" >&2
            ;;
    esac
    if [ -z "$harnesses" ]; then
        say "next: wire it into your harness, e.g. \`agentmail install --harness claude-code\`"
    fi
}

# Everything runs from main, so a download cut off mid-script runs nothing.
main "$@"
