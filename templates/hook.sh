#!/bin/sh
count=$(oca ls --blocked --count)
prev=$(cat "$state_file" 2>/dev/null || echo 0)
[ "$count" = "$prev" ] && exit 0
printf 'oca inbox blocked=%s delta=%+d\n' "$count" "$((count-prev))"
echo "$count" > "$state_file"
