#!/bin/bash
# Breaker #27 scenario: engine and daemons at hard==soft==1024 so the child's fd raise cannot apply.
# usage: hardlimit.sh BIN [SECS] ["8 5" ...]   (wrapper around repro.sh, private PipeWire only)
HARD=1 exec "$(dirname "$(readlink -f "$0")")/repro.sh" "$@"
