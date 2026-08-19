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

target=${NCM_TUI_TARGET:-$(rustc -vV | awk '/^host:/{ print $2; exit }')}
[ -n "$target" ] || {
    echo "could not detect rustc host target" >&2
    exit 1
}
dist_target=${NCM_TUI_DIST_TARGET:-$target}

bin="target/${target}/release/ncm-tui"
if [ ! -x "$bin" ]; then
    bin="target/release/ncm-tui"
fi

if [ "${NCM_TUI_SKIP_BUILD:-}" != "1" ] || [ ! -x "$bin" ]; then
    if [ "$target" = "$(rustc -vV | awk '/^host:/{ print $2; exit }')" ]; then
        cargo build --release
        bin="target/release/ncm-tui"
    else
        cargo build --release --target "$target"
        bin="target/${target}/release/ncm-tui"
    fi
fi

[ -x "$bin" ] || {
    echo "missing binary: $bin" >&2
    exit 1
}

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT INT HUP TERM
cp "$bin" "${stage}/ncm-tui"
chmod 755 "${stage}/ncm-tui"

outdir="${root}/dist/v${version}"
mkdir -p "$outdir"
archive="ncm-tui-${dist_target}.tar.gz"
tar -C "$stage" -czf "${outdir}/${archive}" ncm-tui

sumfile="${outdir}/sha256sums.txt"
touch "$sumfile"
if command -v sha256sum >/dev/null 2>&1; then
    digest=$(sha256sum "${outdir}/${archive}" | awk '{ print $1 }')
else
    digest=$(shasum -a 256 "${outdir}/${archive}" | awk '{ print $1 }')
fi
rest=$(awk -v f="$archive" '$2 != f' "$sumfile")
{
    [ -n "$rest" ] && printf '%s\n' "$rest"
    printf '%s  %s\n' "$digest" "$archive"
} > "${sumfile}.new"
mv "${sumfile}.new" "$sumfile"

printf '%s\n' "$version" > "${root}/dist/latest"
cp "${root}/scripts/install.sh" "${root}/dist/install.sh"
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
  <p><code>curl -fsSL https://mahomaho-rize.com/ncm-tui/install.sh | sh</code></p>
</body>
</html>
EOF

echo "wrote ${outdir}/${archive}"
echo "sha256 ${digest}"
