#!/bin/sh
# Build a release archive for the current host. Writes only artifacts to dist/.
# Does not upload source.

set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$root"

version=$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)
[ -n "$version" ] || {
    echo "could not read version from Cargo.toml" >&2
    exit 1
}

host=$(rustc -vV | awk '/^host:/{ print $2; exit }')
target=${NCM_TUI_TARGET:-$host}
[ -n "$target" ] || {
    echo "could not detect rustc host target" >&2
    exit 1
}
dist_target=${NCM_TUI_DIST_TARGET:-$target}

windows=0
case "$target" in
    *windows*) windows=1 ;;
esac
if [ "$windows" -eq 1 ]; then
    bin_name="ncm-tui.exe"
else
    bin_name="ncm-tui"
fi

py() {
    if command -v python3 >/dev/null 2>&1; then
        python3 "$@"
    else
        python "$@"
    fi
}

bin="target/${target}/release/${bin_name}"
if [ ! -f "$bin" ]; then
    bin="target/release/${bin_name}"
fi

if [ "${NCM_TUI_SKIP_BUILD:-}" != "1" ] || [ ! -f "$bin" ]; then
    if [ "$target" = "$host" ]; then
        cargo build --release
        bin="target/release/${bin_name}"
    else
        cargo build --release --target "$target"
        bin="target/${target}/release/${bin_name}"
    fi
fi

[ -f "$bin" ] || {
    echo "missing binary: $bin" >&2
    exit 1
}

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT INT HUP TERM
cp "$bin" "${stage}/${bin_name}"
if [ "$windows" -eq 0 ]; then
    chmod 755 "${stage}/${bin_name}"
fi

outdir="${root}/dist/v${version}"
mkdir -p "$outdir"
if [ "$windows" -eq 1 ]; then
    archive="ncm-tui-${dist_target}.zip"
    py - "$stage/$bin_name" "$outdir/$archive" "$bin_name" <<'PY'
import sys, zipfile
source, dest, name = sys.argv[1], sys.argv[2], sys.argv[3]
with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as archive:
    archive.write(source, name)
PY
else
    archive="ncm-tui-${dist_target}.tar.gz"
    tar -C "$stage" -czf "${outdir}/${archive}" "$bin_name"
fi

sumfile="${outdir}/sha256sums.txt"
touch "$sumfile"
if command -v sha256sum >/dev/null 2>&1; then
    digest=$(sha256sum "${outdir}/${archive}" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    digest=$(shasum -a 256 "${outdir}/${archive}" | awk '{ print $1 }')
else
    digest=$(py - "$outdir/$archive" <<'PY'
import hashlib, sys
path = sys.argv[1]
hasher = hashlib.sha256()
with open(path, "rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        hasher.update(chunk)
print(hasher.hexdigest())
PY
    )
fi
rest=$(awk -v f="$archive" '$2 != f' "$sumfile")
{
    [ -n "$rest" ] && printf '%s\n' "$rest"
    printf '%s  %s\n' "$digest" "$archive"
} > "${sumfile}.new"
mv "${sumfile}.new" "$sumfile"

printf '%s\n' "$version" > "${root}/dist/latest"
cp "${root}/scripts/install.sh" "${root}/dist/install.sh"
cp "${root}/scripts/install.ps1" "${root}/dist/install.ps1"
chmod 755 "${root}/dist/install.sh"

cat > "${root}/dist/index.html" <<EOF
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>ncm-tui</title>
  <style>
    body { background: #111; color: #eee; font-family: monospace; padding: 3rem; }
    code { color: #9fd; }
  </style>
</head>
<body>
  <p>ncm-tui ${version}</p>
  <p>Unix: <code>curl -fsSL https://mahomaho-rize.com/ncm-tui/install.sh | sh</code></p>
  <p>Windows: <code>irm https://mahomaho-rize.com/ncm-tui/install.ps1 | iex</code></p>
</body>
</html>
EOF

echo "wrote ${outdir}/${archive}"
echo "sha256 ${digest}"
