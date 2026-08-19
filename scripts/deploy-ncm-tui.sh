#!/bin/sh
# Pull a GitHub Release onto the origin. Invoked by Actions via SSM.
# Usage: deploy-ncm-tui 0.1.0
set -eu

VERSION=${1:-}
VERSION=${VERSION#v}
[ -n "$VERSION" ] || {
    echo "usage: $0 <version>" >&2
    exit 1
}

REPO="MahoMaho-Rize/ncm-tui"
DEST="/var/www/ncm-tui"
API="https://api.github.com/repos/${REPO}/releases/tags/v${VERSION}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT HUP TERM

echo "==> fetching v${VERSION} asset list"
python3 - "$API" "$TMP/urls.txt" <<'PY'
import json, sys, urllib.request
url, out = sys.argv[1], sys.argv[2]
req = urllib.request.Request(url, headers={"User-Agent": "ncm-tui-origin-sync"})
with urllib.request.urlopen(req) as response:
    release = json.load(response)
assets = release.get("assets") or []
if not assets:
    raise SystemExit("release has no assets")
with open(out, "w", encoding="utf-8") as handle:
    for asset in assets:
        handle.write(asset["name"] + "\t" + asset["browser_download_url"] + "\n")
PY

mkdir -p "${TMP}/v${VERSION}"
while IFS='	' read -r name url; do
    echo "==> downloading ${name}"
    curl -fL --retry 3 --retry-delay 1 -o "${TMP}/${name}" "$url"
    case "$name" in
        *.tar.gz|*.zip|sha256sums.txt) mv "${TMP}/${name}" "${TMP}/v${VERSION}/${name}" ;;
    esac
done < "${TMP}/urls.txt"

[ -f "${TMP}/install.sh" ] || {
    echo "release is missing install.sh" >&2
    exit 1
}
[ -f "${TMP}/v${VERSION}/sha256sums.txt" ] || {
    echo "release is missing sha256sums.txt" >&2
    exit 1
}

printf '%s\n' "$VERSION" > "${TMP}/latest"
chmod 755 "${TMP}/install.sh"
cat > "${TMP}/index.html" <<EOF
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
  <p>ncm-tui ${VERSION}</p>
  <p>Unix: <code>curl -fsSL https://mahomaho-rize.com/ncm-tui/install.sh | sh</code></p>
  <p>Windows: <code>irm https://mahomaho-rize.com/ncm-tui/install.ps1 | iex</code></p>
</body>
</html>
EOF

echo "==> installing into ${DEST}"
mkdir -p "${DEST}/v${VERSION}"
cp -f "${TMP}/install.sh" "${TMP}/latest" "${TMP}/index.html" "${DEST}/"
if [ -f "${TMP}/install.ps1" ]; then
    cp -f "${TMP}/install.ps1" "${DEST}/"
fi
cp -f "${TMP}/v${VERSION}/"* "${DEST}/v${VERSION}/"
chmod 755 "${DEST}" "${DEST}/install.sh"
find "${DEST}" -type f ! -name install.sh -exec chmod 644 {} +

echo "==> health check"
curl --fail --silent --show-error "http://127.0.0.1:8080/ncm-tui/install.sh" | head -n 1 | grep -q '^#!'
curl --fail --silent --show-error "http://127.0.0.1:8080/ncm-tui/latest" | grep -qx "${VERSION}"
echo "==> origin now serving ncm-tui ${VERSION}"
