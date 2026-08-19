#!/bin/sh
# Install ncm-tui from prebuilt archives.
#
#   curl -fsSL https://mahomaho-rize.com/ncm-tui/install.sh | sh
#
# Optional environment:
#   NCM_TUI_VERSION       pin a version, e.g. 0.1.0 (default: latest)
#   NCM_TUI_INSTALL_DIR   install directory (default: ~/.local/bin)
#   NCM_TUI_BASE_URL      artifact base (default: https://mahomaho-rize.com/ncm-tui)

set -eu

APP="ncm-tui"
BASE_URL="${NCM_TUI_BASE_URL:-https://mahomaho-rize.com/ncm-tui}"
VERSION="${NCM_TUI_VERSION:-}"
BIN_DIR="${NCM_TUI_INSTALL_DIR:-${HOME}/.local/bin}"

info() {
    printf '%s\n' "$*"
}

err() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) printf 'aarch64-apple-darwin\n' ;;
                *) err "ncm-tui only supports Apple Silicon Macs, not $arch" ;;
            esac
            return
            ;;
        Linux)
            case "$arch" in
                x86_64|amd64) cpu="x86_64" ;;
                arm64|aarch64) cpu="aarch64" ;;
                *) err "unsupported architecture: $arch" ;;
            esac
            os_tag="unknown-linux-gnu"
            if [ -r /etc/os-release ]; then
                # shellcheck disable=SC1091
                . /etc/os-release
                case "${ID:-}" in
                    fedora)
                        if [ "$cpu" = "x86_64" ]; then
                            os_tag="unknown-linux-fedora"
                        fi
                        ;;
                esac
            fi
            printf '%s-%s\n' "$cpu" "$os_tag"
            return
            ;;
        MINGW*|MSYS*|CYGWIN*|Windows_NT)
            printf 'x86_64-pc-windows-msvc\n'
            return
            ;;
        *) err "unsupported OS: $os (supported: macOS Apple Silicon, Linux, Windows)" ;;
    esac
}

digest_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        err "need sha256sum or shasum"
    fi
}

need_cmd uname
need_cmd mkdir
need_cmd curl
need_cmd mktemp
need_cmd awk
need_cmd mv
need_cmd chmod
need_cmd cp

target=$(detect_target)

if [ -z "$VERSION" ]; then
    VERSION=$(curl -fsSL "${BASE_URL}/latest") || err "could not read ${BASE_URL}/latest"
    VERSION=$(printf '%s' "$VERSION" | tr -d ' \t\r\n')
    [ -n "$VERSION" ] || err "empty version from ${BASE_URL}/latest"
fi
VERSION=${VERSION#v}

case "$target" in
    *windows*)
        archive="${APP}-${target}.zip"
        installed_name="${APP}.exe"
        ;;
    *)
        need_cmd tar
        archive="${APP}-${target}.tar.gz"
        installed_name="${APP}"
        ;;
esac
url="${BASE_URL}/v${VERSION}/${archive}"
sums_url="${BASE_URL}/v${VERSION}/sha256sums.txt"

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT INT HUP TERM

info "installing ${APP} ${VERSION} (${target})"
curl -fL --retry 3 --retry-delay 1 -o "${tmpdir}/${archive}" "$url" \
    || err "no prebuilt archive for ${target} at ${url}"
curl -fL --retry 3 --retry-delay 1 -o "${tmpdir}/sha256sums.txt" "$sums_url" \
    || err "could not download ${sums_url}"

expected=$(awk -v f="$archive" '$2 == f { print $1; exit }' "${tmpdir}/sha256sums.txt")
[ -n "$expected" ] || err "no checksum for ${archive} in sha256sums.txt"
actual=$(digest_file "${tmpdir}/${archive}")
[ "$expected" = "$actual" ] || err "checksum mismatch for ${archive}"

case "$archive" in
    *.zip)
        if command -v unzip >/dev/null 2>&1; then
            unzip -qo "${tmpdir}/${archive}" -d "$tmpdir"
        else
            tar -C "$tmpdir" -xf "${tmpdir}/${archive}"
        fi
        ;;
    *)
        tar -C "$tmpdir" -xzf "${tmpdir}/${archive}"
        ;;
esac
[ -f "${tmpdir}/${installed_name}" ] || err "archive did not contain ${installed_name}"

mkdir -p "$BIN_DIR"
cp "${tmpdir}/${installed_name}" "${BIN_DIR}/${installed_name}.new"
chmod 755 "${BIN_DIR}/${installed_name}.new"
mv -f "${BIN_DIR}/${installed_name}.new" "${BIN_DIR}/${installed_name}"

info "installed ${BIN_DIR}/${installed_name}"
case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *)
        info "add ${BIN_DIR} to PATH, for example:"
        info "  echo 'export PATH=\"${BIN_DIR}:\$PATH\"' >> ~/.zshrc"
        ;;
esac
