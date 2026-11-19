#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Builds the archive sent to extensions.gnome.org.
#
#   extension/scripts/pack.sh [output dir]     (default: extension/dist)
#
# Only what the Shell runs goes in: no tools/, scripts/ or README. Nothing is
# installed. Run shexli on the result before any upload.
set -euo pipefail

EXT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$EXT/dist}"
mkdir -p "$OUT"

# The extension carries the D-Bus contract: it must be the contract.
"$EXT/tools/update-contract.py" --check

gnome-extensions pack "$EXT" \
    --extra-source=caseCard.js \
    --extra-source=contract.js \
    --extra-source=engine.js \
    --extra-source=engine-release.json \
    --extra-source=headView.js \
    --extra-source=icons.js \
    --extra-source=installer.js \
    --extra-source=model.js \
    --extra-source=notify.js \
    --extra-source=osd.js \
    --extra-source=rings.js \
    --extra-source=spatialPage.js \
    --extra-source=testSound.js \
    --extra-source=tile.js \
    --extra-source=icons \
    --extra-source=art \
    --extra-source=stylesheet-dark.css \
    --extra-source=stylesheet-light.css \
    --extra-source=COPYING \
    --force --out-dir "$OUT"
echo "$OUT/pods@d101c.github.io.shell-extension.zip"
