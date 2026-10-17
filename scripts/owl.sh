#!/bin/bash
# Phase 1: builds OWL (the AWDL daemon) at a pinned commit of the fork proven
# on the MT7921, and installs it root-owned where `lindrop radio` looks.
set -euo pipefail

REPO=https://github.com/jedbillyb/owl.git
COMMIT=832d70f815c3d4a06a02117bf0fc5e868daa1ff0
SRC="${XDG_CACHE_HOME:-$HOME/.cache}/lindrop/owl"
PATCHES="$(cd "$(dirname "$(readlink -f "$0")")/../patches" && pwd)"
DEST=/usr/local/lib/lindrop/owl

if [ ! -d "$SRC/.git" ]; then
  git clone --quiet "$REPO" "$SRC"
fi
git -C "$SRC" fetch --quiet origin
git -C "$SRC" checkout --quiet --force "$COMMIT"
git -C "$SRC" submodule update --quiet --init --recursive
# Our fixes on top (patches/owl-*.patch, GPL like OWL), on a clean tree.
git -C "$SRC" reset --quiet --hard
for p in "$PATCHES"/owl-*.patch; do
  git -C "$SRC" apply "$p" || { echo "patch $(basename "$p") non applicata"; exit 1; }
done
# The bundled googletest declares a CMake minimum newer CMake refuses.
cmake -S "$SRC" -B "$SRC/build" -DCMAKE_BUILD_TYPE=Release -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -Wno-deprecated >/dev/null
# Only the daemon: the bundled googletest does not build with current GCC.
cmake --build "$SRC/build" --target owl -j"$(nproc)" >/dev/null
sudo install -D -o root -g root -m 755 "$SRC/build/daemon/owl" "$DEST"
echo "OWL installato in $DEST (commit ${COMMIT:0:7} + $(ls "$PATCHES"/owl-*.patch | wc -l) patch)"
