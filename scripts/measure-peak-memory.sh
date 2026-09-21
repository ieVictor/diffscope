#!/usr/bin/env bash
set -euo pipefail

if (($# == 0)); then
  printf 'usage: %s COMMAND [ARG ...]\n' "$0" >&2
  exit 2
fi

"$@" >/dev/null &
pid=$!
peak_kib=0
while kill -0 "$pid" 2>/dev/null; do
  if rss_kib=$(awk '/^VmHWM:/{print $2}' "/proc/$pid/status" 2>/dev/null); then
    if [[ -n $rss_kib ]] && ((rss_kib > peak_kib)); then
      peak_kib=$rss_kib
    fi
  fi
  sleep 0.01
done

set +e
wait "$pid"
status=$?
set -e
printf 'peak_rss_kib: %d\n' "$peak_kib"
exit "$status"
