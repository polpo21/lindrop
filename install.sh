#!/bin/bash
# Installs or updates lindrop. Run as your normal user; it uses sudo where needed.
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
[ "$(id -u)" != 0 ] || { echo "lancialo come utente normale, non come root"; exit 1; }

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

step "Dipendenze"
if command -v pacman >/dev/null; then
  sudo pacman -S --needed --noconfirm cmake libev libpcap libnl iw
else
  echo "Installa a mano: cmake, libev, libpcap, libnl, iw"
fi

step "Compilo lindrop"
cargo build --release --locked
sudo install -o root -g root -m 755 target/release/lindrop /usr/local/bin/lindrop

step "OWL (AWDL, fase 1)"
./scripts/owl.sh

step "Servizio radio"
sudo install -D -m 644 packaging/lindrop-radio.socket /etc/systemd/system/lindrop-radio.socket
sudo install -D -m 644 packaging/lindrop-radio.service /etc/systemd/system/lindrop-radio.service
sudo install -D -m 644 packaging/10-lindrop-awdl0.link /etc/systemd/network/10-lindrop-awdl0.link
sudo install -d -m 755 /etc/lindrop
grep -qx "$USER" /etc/lindrop/utenti 2>/dev/null || echo "$USER" | sudo tee -a /etc/lindrop/utenti >/dev/null
sudo systemctl daemon-reload
sudo udevadm control --reload
sudo systemctl enable --now lindrop-radio.socket
# A running service keeps the old binary: restart it if idle.
sudo systemctl try-restart lindrop-radio.service

step "Barra (Noctalia) e servizio utente"
# User-level, no sudo: the unit the bar switch starts and stops, and the plugin.
mkdir -p "$HOME/.config/systemd/user" "$HOME/.local/share/noctalia/plugins"
ln -sfn "$PWD/packaging/lindrop-ricevi.service" "$HOME/.config/systemd/user/lindrop-ricevi.service"
systemctl --user daemon-reload
ln -sfn "$PWD/integrazioni/noctalia/lindrop" "$HOME/.local/share/noctalia/plugins/lindrop"
if command -v noctalia >/dev/null && noctalia msg plugins list >/dev/null 2>&1; then
  noctalia msg plugins enable polpo/lindrop >/dev/null && echo "  plugin polpo/lindrop attivo"
  noctalia config export 2>/dev/null | grep -q 'polpo/lindrop:airdrop' \
    || echo "  per l'icona: aggiungi un widget di tipo \"polpo/lindrop:airdrop\" alla barra"
fi

step "Fatto"
echo "  lindrop ricevi        per ricevere dall'iPhone"
echo "  lindrop invia FILE    per inviare all'iPhone"
