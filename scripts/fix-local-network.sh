#!/bin/sh
# Restore the daemon's Local Network access after a rebuild.
#
# macOS pins the Local Network privacy grant to the binary's code signature;
# a rebuild changes it and nehelper keeps denying from a stale cache until it
# restarts (normally only at boot). Killing nehelper forces the re-bind, then
# the kickstart respawns the daemon with the refreshed grant.
set -eu

sudo pkill -9 nehelper
sleep 2 # let launchd respawn nehelper before the daemon reconnects
launchctl kickstart -k "gui/$(id -u)/apps.andersou.geekmagic-custom-monitors"
