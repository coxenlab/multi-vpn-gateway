#!/bin/sh
# Locked sources and matching guestagent; the gvisor698 candidate is explicitly selected.
set -eu
cd "$(dirname "$0")"
exec python3 ./build-runtime.py --variant "${VPNMGR_RUNTIME_VARIANT:-baseline}"
