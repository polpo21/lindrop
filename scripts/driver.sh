#!/bin/bash
# Builds mt7921e with driver/mt7921e-rate.patch for the running kernel and
# installs it in /lib/modules/<kernel>/updates/lindrop/, which modprobe
# prefers over the stock driver. Only mt7921e.ko is rebuilt; the rest of
# mt76 stays stock. A new kernel has no copy there and loads the stock
# driver again: rerun this script after kernel updates.
#
#   scripts/driver.sh            build, install, reload the driver
#   scripts/driver.sh --rimuovi  back to the stock driver
set -euo pipefail

KVER=$(uname -r)
REPO="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/lindrop/kernel"
DEST=/lib/modules/$KVER/updates/lindrop/mt7921e.ko
KBUILD=/lib/modules/$KVER/build

reload() {
  echo "Ricarico il driver (il Wi-Fi cade per qualche secondo)…"
  sudo modprobe -r mt7921e && sudo modprobe mt7921e
}

if [ "${1:-}" = "--rimuovi" ]; then
  sudo rm -f "$DEST" && sudo depmod "$KVER"
  reload
  echo "Driver di serie ripristinato."
  exit 0
fi

[ -d "$KBUILD" ] || { echo "mancano le intestazioni del kernel $KVER (pacchetto *-headers)"; exit 1; }
# Never work around module signing: if the kernel insists on signed modules,
# stop here and say so.
if [ "$(cat /sys/module/module/parameters/sig_enforce 2>/dev/null)" = "Y" ]; then
  echo "Il kernel accetta solo moduli firmati: non installo un driver modificato."
  exit 1
fi

# The exact source of this kernel: CachyOS tags its tree cachyos-<ver>-<rel>.
case "$KVER" in
  *-cachyos) URL=https://github.com/CachyOS/linux.git; TAG="cachyos-${KVER%-cachyos}" ;;
  *)         URL=https://github.com/gregkh/linux.git;  TAG="v${KVER%%-*}" ;;
esac
SRC="$CACHE/$TAG"
if [ ! -d "$SRC/.git" ]; then
  echo "Scarico i sorgenti di mt76 ($TAG)…"
  git clone --quiet --filter=blob:none --sparse --depth 1 --branch "$TAG" "$URL" "$SRC"
  git -C "$SRC" sparse-checkout set drivers/net/wireless/mediatek/mt76
fi
git -C "$SRC" checkout --quiet -- .
git -C "$SRC" apply "$REPO/driver/mt7921e-rate.patch"

# Out-of-tree build of mt7921e only, with mt76's headers beside it.
B=$(mktemp -d)
trap 'rm -rf "$B"' EXIT
M="$SRC/drivers/net/wireless/mediatek/mt76"
mkdir -p "$B/mt76/mt7921"
cp "$M"/*.h "$B/mt76/"
cp "$M"/mt7921/*.h "$M"/mt7921/pci.c "$M"/mt7921/pci_mac.c "$M"/mt7921/pci_mcu.c "$B/mt76/mt7921/"
printf 'obj-m := mt7921e.o\nmt7921e-y := pci.o pci_mac.o pci_mcu.o\n' > "$B/mt76/mt7921/Kbuild"
LLVM=
grep -q '^CONFIG_CC_IS_CLANG=y' "$KBUILD/.config" && LLVM=1
make -s -C "$KBUILD" M="$B/mt76/mt7921" ${LLVM:+LLVM=1} modules 2>&1 | grep -v 'compiler differs\|built by\|You are using' || true
[ -f "$B/mt76/mt7921/mt7921e.ko" ] || { echo "compilazione fallita"; exit 1; }

sudo install -D -m 644 "$B/mt76/mt7921/mt7921e.ko" "$DEST"
sudo depmod "$KVER"
echo "Installato $DEST"
reload
echo "Fatto. Per tornare al driver di serie: $0 --rimuovi"
