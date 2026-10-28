#!/bin/sh
# agentmail installer for Linux and WSL.
#
#   curl -fsSL https://raw.githubusercontent.com/quinnjr/agentmail/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/quinnjr/agentmail/main/install.sh | sh -s -- --harness auto
#
# Downloads the prebuilt static binary for this machine from the GitHub
# release, checks its SHA-256, and installs it into ~/.local/bin. Builds the
# release from source with cargo when there is no prebuilt binary for this
# CPU (or with --from-source). Run with --help for the options.

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

# download <url> <file>: 0 on success, 2 when the file does not exist there
# (HTTP 404, or a missing file:// path), 1 on any other failure.
download() {
    if has curl; then
        code="$(curl --proto '=https,file' --tlsv1.2 -sSL -o "$2" -w '%{http_code}' "$1")" || {
            rc=$?
            [ "$rc" = 37 ] && return 2 # file:// path that does not exist
            return 1
        }
        case "$code" in
            2?? | 000) return 0 ;; # 000: a file:// URL, which has no status
            404) return 2 ;;
            *) say "$1: HTTP $code"; return 1 ;;
        esac
    else
        wget -q -O "$2" "$1" || {
            rc=$?
            [ "$rc" = 8 ] && return 2 # the server answered with an error
            return 1
        }
    fi
}

sha256_of() {
    if has sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# latest_tag <base>: where GitHub's releases/latest redirects. Prints the
# tag of the latest release, `none` when there is no release yet (GitHub
# then redirects to the release list), or nothing when it cannot tell (a
# network error, or a mirror without the redirect).
latest_tag() {
    if has curl; then
        url="$(curl --proto '=https,file' -sSLo /dev/null -w '%{url_effective}' "$1/latest" 2>/dev/null)" || return 0
    else
        url="$(wget --max-redirect=0 -S -O /dev/null "$1/latest" 2>&1 | sed -n 's/^ *Location: *//p' | tail -n1 | tr -d '\r')"
    fi
    case "$url" in
        */releases/tag/*) printf '%s\n' "${url##*/}" ;;
        */releases | */releases/) echo none ;;
        *) ;;
    esac
}

# has_linux <cmd>: <cmd> is on PATH outside the Windows drives WSL mounts,
# so it is not a Windows tool seen through WSL's appended Windows PATH.
has_linux() {
    p="$(command -v "$1" 2>/dev/null)" || return 1
    case "$p" in
        /mnt/*) return 1 ;;
    esac
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
# release archive into <tmpdir>/unpacked. Returns 2 when the release has no
# archive for <triple>; dies on every other failure.
fetch_prebuilt() {
    archive="agentmail-$2.tar.gz"
    if [ "$tag" = latest ]; then
        url="$base/latest/download/$archive"
    else
        url="$base/download/$tag/$archive"
    fi
    say "downloading $url"
    rc=0
    download "$url" "$1/$archive" || rc=$?
    [ "$rc" = 2 ] && return 2
    [ "$rc" = 0 ] || die "could not download $url"
    download "$url.sha256" "$1/$archive.sha256" ||
        die "the release has no checksum for $archive; refusing to install it unverified"
    want="$(cut -d' ' -f1 <"$1/$archive.sha256")"
    case "$want" in
        *[!0-9a-f]* | "") die "the checksum file for $archive is not a SHA-256 hash" ;;
    esac
    [ "${#want}" = 64 ] || die "the checksum file for $archive is not a SHA-256 hash"
    got="$(sha256_of "$1/$archive")" || die "could not hash $archive"
    [ "$want" = "$got" ] || die "checksum mismatch for $archive (expected $want, got $got)"
    mkdir "$1/unpacked" || die "could not create $1/unpacked"
    tar -xzf "$1/$archive" -C "$1/unpacked" || die "could not unpack $archive"
    [ -f "$1/unpacked/agentmail" ] || die "$archive has no agentmail binary"
}

# build_from_source <tmpdir>: cargo install of the release into
# <tmpdir>/root. With --from-source and no release published yet, of the
# default branch.
build_from_source() {
    has cargo || die "cargo is not installed, so agentmail cannot be built from source.
Install Rust from https://rustup.rs and run this again, or use a release from
https://github.com/$REPO/releases"
    if [ "$tag" = latest ]; then
        [ "$release" = none ] || die "could not find the latest release to build; pass --version <tag>"
        [ "$from_source" = 1 ] || die "no agentmail release has been published yet.
Run this again with --from-source to build the development branch."
        say "no release published yet; building the development branch"
        set -- "$1" --git "https://github.com/$REPO"
    else
        set -- "$1" --git "https://github.com/$REPO" --tag "$tag"
    fi
    say "building from source with cargo (this takes a few minutes)"
    root="$1"
    shift
    cargo install --locked "$@" --root "$root/root" agentmail || die "cargo install failed"
    mkdir "$root/unpacked" || die "could not create $root/unpacked"
    cp "$root/root/bin/agentmail" "$root/unpacked/agentmail" || die "cargo installed no agentmail binary"
}

# on_path <dir>: whether <dir> is on the user's PATH. Under WSL, also as
# their login shell sets it, since `wsl.exe -e` starts without the profile:
# detached from the terminal (no passphrase prompts) and time-limited.
on_path() {
    case ":$user_path:" in *":$1:"*) return 0 ;; esac
    if [ -n "${WSL_DISTRO_NAME:-}" ] && has setsid && has timeout; then
        login_path="$(PATH="$user_path" timeout 5 setsid "${SHELL:-/bin/sh}" -lc 'echo "$PATH"' </dev/null 2>/dev/null || true)"
        # fish prints its PATH list space-separated.
        case ":$login_path: " in *":$1:"* | *" $1 "*) return 0 ;; esac
    fi
    return 1
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
        "" | v) die "--version needs a value" ;;
        latest | v*) ;;
        *) version="v$version" ;;
    esac
    # Harness names are words: no globbing when splitting the list.
    set -f
    for h in $harnesses; do
        case "$h" in
            claude-code | opencode | kiro | auto) ;;
            *) die "unknown harness '$h' (expected claude-code, opencode, kiro or auto)" ;;
        esac
    done
    set +f
    case "$bin_dir" in
        "") die "--bin-dir needs a value" ;;
        "~") bin_dir="$HOME" ;;
        "~/"*) bin_dir="$HOME/${bin_dir#"~/"}" ;;
        /*) ;;
        *) bin_dir="$(pwd)/$bin_dir" ;;
    esac
    while :; do
        case "$bin_dir" in
            / | *[!/]) break ;;
            */) bin_dir="${bin_dir%/}" ;;
        esac
    done

    check_platform
    has sha256sum || has shasum || die "need sha256sum or shasum to verify the download"
    has curl || has wget || die "need curl or wget to download"

    # Find tools where they are usually installed even when this shell's PATH
    # lacks them (wsl.exe -e and other non-login shells skip ~/.profile).
    user_path="$PATH"
    PATH="$bin_dir:$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
    export PATH

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    base="${AGENTMAIL_DOWNLOAD_URL:-https://github.com/$REPO/releases}"
    tag="$version"
    release=""
    if [ "$tag" = latest ]; then
        # One tag for every download (and a source build), so a release
        # published meanwhile cannot mix archives and checksums.
        release="$(latest_tag "$base")"
        case "$release" in
            none)
                [ "$from_source" = 1 ] || die "no agentmail release has been published yet.
Run this again with --from-source to build the development branch." ;;
            "") ;;
            *) tag="$release" ;;
        esac
    fi

    triple="$(target_triple)"
    if [ "$from_source" = 1 ]; then
        build_from_source "$tmp"
    elif [ -z "$triple" ]; then
        say "no prebuilt binary for $(uname -m)"
        build_from_source "$tmp"
    else
        rc=0
        fetch_prebuilt "$tmp" "$triple" || rc=$?
        if [ "$rc" = 2 ]; then
            [ "$tag" = latest ] && die "found no agentmail release at $base"
            die "release $tag has no agentmail-$triple.tar.gz (see https://github.com/$REPO/releases)"
        fi
    fi

    new="$tmp/unpacked/agentmail"
    mkdir -p "$bin_dir" || die "could not create $bin_dir"
    bin="$bin_dir/agentmail"
    # Copy next to the target, check it runs there (the temp dir may be
    # noexec), then rename it over the old one: a running daemon keeps its
    # old binary and nothing ever sees a half-written file.
    cp "$new" "$bin.new" || die "could not write $bin.new"
    chmod 755 "$bin.new"
    installed="$("$bin.new" --version 2>/dev/null)" || {
        rm -f "$bin.new"
        die "the downloaded agentmail does not run on this machine"
    }
    mv -f "$bin.new" "$bin" || die "could not replace $bin"
    say "installed $installed to $bin"

    # A daemon of an older binary may still be running, maybe speaking an
    # older protocol: stop it so the next command starts this one (under
    # systemd, the unit restarts it). Stopping when none runs does nothing.
    if ! "$bin" daemon stop >/dev/null 2>&1; then
        say "warning: could not stop the running daemon; run \`agentmail daemon stop\`"
    fi

    other="$(PATH="$user_path" command -v agentmail 2>/dev/null || true)"
    if [ -n "$other" ] && ! [ "$other" -ef "$bin" ]; then
        say "warning: your PATH finds another agentmail first, at $other"
        say "remove it (e.g. \`cargo uninstall agentmail\`) or put $bin_dir earlier on PATH"
    fi

    set -f
    for h in $harnesses; do
        if [ "$h" = auto ]; then
            found=0
            if has_linux claude; then "$bin" install --harness claude-code; found=1; fi
            if has_linux opencode; then "$bin" install --harness opencode; found=1; fi
            if has_linux kiro-cli; then "$bin" install --harness kiro; found=1; fi
            [ "$found" = 1 ] || say "--harness auto: no claude, opencode or kiro-cli on PATH"
        else
            "$bin" install --harness "$h"
        fi
    done
    set +f
    if [ "$service" = 1 ]; then
        "$bin" install --service
    fi

    if ! on_path "$bin_dir"; then
        say "$bin_dir is not on your PATH; add this to your shell profile:"
        printf '\n    export PATH="%s:$PATH"\n\n' "$bin_dir" >&2
    fi
    if [ -z "$harnesses" ]; then
        say "next: wire it into your harness, e.g. \`agentmail install --harness claude-code\`"
    fi
}

# Everything runs from main, so a download cut off mid-script runs nothing.
main "$@"
