#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Builds the release archive of the engine for one architecture, the one the
# extension's "Install Engine" downloads, and its SHA-256. Publishes nothing.
#
#   scripts/make-release.sh                     this machine's architecture
#   scripts/make-release.sh --arch aarch64      cross-build (needs the Rust
#                                               target and a linker for it)
#   scripts/make-release.sh --no-build          package what target/ holds
#
# Writes to engine/dist/:
#   pods-engine-<version>-<arch>.tar.gz         pods-engine-<version>-<arch>/
#                                                 podsd, podsctl,
#                                                 pods-engine.service,
#                                                 io.github.d101c.Pods.service,
#                                                 COPYING, NOTICE, README.md
#   pods-engine-<version>-<arch>.tar.gz.sha256  as sha256sum writes it
# and prints the entry of extension/engine-release.json for this archive.
#
# The archive is reproducible: same sources and toolchain, same bytes (sorted
# names, owner 0, timestamps from the last commit, gzip without a name).
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
arch=$(uname -m)
build=yes
while [ $# -gt 0 ]; do
    case "$1" in
        --arch) arch="$2"; shift 2 ;;
        --no-build) build=no; shift ;;
        *) echo "usage: $0 [--arch x86_64|aarch64] [--no-build]" >&2; exit 2 ;;
    esac
done
case "$arch" in
    x86_64|aarch64) ;;
    *) echo "unsupported architecture: $arch (x86_64 or aarch64)" >&2; exit 2 ;;
esac

version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$here/Cargo.toml")
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }
name="pods-engine-$version-$arch"

if [ "$arch" = "$(uname -m)" ]; then
    target_dir="$here/target/release"
    target_flag=""
else
    target_dir="$here/target/$arch-unknown-linux-gnu/release"
    target_flag="--target $arch-unknown-linux-gnu"
fi
if [ "$build" = yes ]; then
    # shellcheck disable=SC2086
    (cd "$here" && cargo build --release --locked $target_flag)
fi
for bin in podsd podsctl; do
    [ -x "$target_dir/$bin" ] || { echo "missing $target_dir/$bin" >&2; exit 1; }
done

out="$here/dist"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$out" "$stage/$name"
chmod 0755 "$stage/$name"
install -m 0755 "$target_dir/podsd" "$target_dir/podsctl" "$stage/$name/"
install -m 0644 "$here/data/pods-engine.service" "$here/data/io.github.d101c.Pods.service" \
    "$here/COPYING" "$here/NOTICE" "$here/README.md" "$stage/$name/"

epoch=$(git -C "$here" log -1 --format=%ct 2>/dev/null || echo 0)
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$epoch" \
    --format=gnu -C "$stage" -cf - "$name" | gzip -9 -n > "$out/$name.tar.gz"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")

sha=$(cut -d' ' -f1 "$out/$name.tar.gz.sha256")
size=$(stat -c %s "$out/$name.tar.gz")
echo "$out/$name.tar.gz"
echo "$out/$name.tar.gz.sha256"
cat <<EOF

extension/engine-release.json, "version": "$version", "assets":
    "$arch": {
      "url": "https://github.com/d101c/pods/releases/download/engine-$version/$name.tar.gz",
      "size": $size,
      "sha256": "$sha"
    }
EOF
