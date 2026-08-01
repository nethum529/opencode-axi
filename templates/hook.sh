#!/bin/sh
count=$(oca ls --blocked --count)
prev=$(cat "$state_file" 2>/dev/null || echo 0)
[ "$count" = "$prev" ] && exit 0
printf 'oca inbox blocked=%s delta=%+d refs=%s\n' "$count" "$((count-prev))" "$(oca ls --blocked --count --refs)"
echo "$count" > "$state_file"
