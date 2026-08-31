#!/bin/sh
# Diagnose the daemon's Local Network access on macOS.
#
# This script does NOT fix anything, because for this binary there is nothing to
# reset. Everything below was measured on Darwin 25.6, not assumed.
#
# The grant is keyed to the binary's ad-hoc *signing identifier*, which for a
# Rust binary is `<crate>-<metadata hash>` (see `codesign -d`). That hash comes
# from the crate version, dependency graph, features and toolchain -- NOT from
# the source. Ordinary code edits keep the identity, so they keep working; a
# version bump or a dependency change mints a brand new identity with no grant,
# and every LAN connection then fails with `No route to host` (EHOSTUNREACH,
# logged by the kernel as `reason: NECP`).
#
# A bundle-less executable can never be approved by hand: nehelper logs
# `Could not find bundle ID or display name for app`, so System Settings ->
# Privacy & Security -> Local Network never grows a row to toggle.
#
# Measured to NOT help:
#   * launchctl kickstart -k
#   * launchctl bootout + bootstrap (re-registering the job)
#   * running the binary in the foreground first (that borrows Terminal's grant,
#     which does not transfer to the launchd job)
#   * re-signing ad-hoc with a fixed --identifier
#   * tccutil (Local Network is not a TCC service)
#   * restarting nehelper: it is spawned on demand and a brand new instance
#     already denies the unknown identity, so there is no stale cache to clear
#
# Observed to work: a reboot, or giving the binary a nameable code identity
# (a minimal ad-hoc signed .app wrapper, which makes the grant survive rebuilds).
set -eu

LABEL=apps.andersou.geekmagic-custom-monitors
EXE=$(plutil -extract ProgramArguments.0 raw \
    "$HOME/Library/LaunchAgents/$LABEL.plist" 2>/dev/null || echo "")
CONFIG="$HOME/.config/geekmagic-custom-monitors/config.toml"

if [ -z "$EXE" ]; then
    echo "daemon: not enabled (no plist); run 'geekmagic-monitors daemon enable'"
    exit 0
fi

echo "daemon binary:  $EXE"
codesign -d --verbose=2 "$EXE" 2>&1 | sed -n 's/^Identifier=/identity:       /p'
echo "note:           a changed identity above means a lost grant"

HOST=$(sed -n 's/^[[:space:]]*host[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
    "$CONFIG" 2>/dev/null | head -1)
if [ -n "$HOST" ]; then
    if curl -s -m 5 -o /dev/null "http://$HOST/"; then
        echo "from this shell: $HOST reachable (Terminal holds a grant)"
    else
        echo "from this shell: $HOST unreachable (check the device itself)"
    fi
fi

echo
echo "last daemon log lines:"
tail -5 "$HOME/Library/Logs/geekmagic-custom-monitors.log" 2>/dev/null \
    || echo "  (no log yet)"
