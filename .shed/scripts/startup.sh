#!/usr/bin/env bash
# Per-boot startup hook: make the kernel input device + the boot seatd socket
# reachable by the (non-root, non-`video`-group) shed user, so the cage+uinput
# Wayland pointer-drag guard can inject. Both reset to root-only on every boot;
# `shed exec` doesn't pick up `video`-group membership, so we open the nodes
# directly. No-ops cleanly when uinput/seatd aren't present, so it's harmless on
# sheds that never run the drag test.
set -uo pipefail
sudo modprobe uinput 2>/dev/null || true
[ -e /dev/uinput ] && sudo chmod 0666 /dev/uinput || true
# seatd.service is enabled but can be inactive right after the install hook
# installs it (and races systemd on a fresh boot). Start it (idempotent), wait
# for its socket, then open it — the shed user isn't in the `video` group seatd
# runs as, and `shed exec` doesn't pick up group changes.
sudo systemctl start seatd 2>/dev/null || true
for _ in $(seq 1 20); do [ -S /run/seatd.sock ] && break; sleep 0.3; done
[ -S /run/seatd.sock ] && sudo chmod 0666 /run/seatd.sock || true
exit 0
