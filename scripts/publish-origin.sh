#!/bin/sh
# Copy release artifacts (install.sh + tarballs) to the origin.
# Never uploads the git tree, Cargo sources, or local config.

set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
dist="${root}/dist"
key="${NCM_TUI_SSH_KEY:-${HOME}/Downloads/ec-tokyo.pem}"
host="${NCM_TUI_ORIGIN:-ec2-user@18.183.251.184}"

[ -f "${dist}/install.sh" ] && [ -f "${dist}/latest" ] || {
    echo "run scripts/package.sh first" >&2
    exit 1
}
[ -f "$key" ] || {
    echo "missing SSH key: $key" >&2
    exit 1
}

ssh_cmd="ssh -i ${key} -o StrictHostKeyChecking=accept-new"
remote_tmp="/tmp/ncm-tui-dist"

echo "==> uploading artifacts to ${host}"
rsync -a --delete -e "$ssh_cmd" "${dist}/" "${host}:${remote_tmp}/"

$ssh_cmd "$host" "set -eu
    sudo mkdir -p /var/www/ncm-tui
    sudo rsync -a --delete ${remote_tmp}/ /var/www/ncm-tui/
    sudo chmod 755 /var/www/ncm-tui /var/www/ncm-tui/install.sh
    sudo find /var/www/ncm-tui -type f -exec chmod 644 {} +
    sudo chmod 755 /var/www/ncm-tui/install.sh
    rm -rf ${remote_tmp}
"

echo "==> published"
echo "    curl -fsSL https://mahomaho-rize.com/ncm-tui/install.sh | sh"
