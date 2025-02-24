#! /bin/bash

# set -x
set -euo pipefail

if [[ $# -ne 0 ]]; then
    echo "$0: expect no arguments" >&2
    exit 1
fi

cargo license |
while read -r X; do
    echo "$X" | grep -w 'Apache\|BSD-3-Clause\|ISC\|MIT\|N/A'
done
