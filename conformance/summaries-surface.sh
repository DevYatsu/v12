#!/usr/bin/env bash
# Print the `summary:` line for each conformance report in a directory.
set -uo pipefail
for f in Set Promise Iterator Symbol Error Object String language; do
  if [ -f "/tmp/$1/$f.json" ]; then
    printf '%-10s ' "$f"
    grep -o '"summary":{[^}]*}' "/tmp/$1/$f.json" | head -c 200
    echo
  fi
done
