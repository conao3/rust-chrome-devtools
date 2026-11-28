#!/bin/sh
printf '%s\n' "$@" > "$FAKE_CHROME_DUMP.args"
tr '\0' '\n' < /proc/$$/environ > "$FAKE_CHROME_DUMP.tmp"
mv "$FAKE_CHROME_DUMP.tmp" "$FAKE_CHROME_DUMP"
