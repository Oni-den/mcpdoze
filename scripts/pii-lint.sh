#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# PII and secret lint for tracked files (git ls-files). Prints file:line and
# the rule that matched. Never prints the matched text, so CI logs stay clean.
# Exits 1 when any finding is reported.
set -u

root=$(git rev-parse --show-toplevel) || exit 2
cd "$root" || exit 2

fail=0

# Rules that allow a match: user-profile names, email domains, IPs.
is_allowed_user() {
  local name lower
  name=$(printf '%s' "$1" | sed -E 's#^.*(Users[\\/]+|/home/|/Users/)##I')
  case "$name" in
    "" | "<"*) return 0 ;;
  esac
  lower=${name,,}
  case "$lower" in
    example | user | runner | username) return 0 ;;
  esac
  return 1
}

is_allowed_email() {
  local value local_part domain
  value=${1,,}
  local_part=${value%@*}
  domain=${value#*@}
  case "$local_part" in
    *noreply*) return 0 ;;
  esac
  case "$domain" in
    example.com | users.noreply.github.com) return 0 ;;
  esac
  return 1
}

# scan RULE ERE GREP_FLAGS FILTER_FN
# Reports each offending line once per rule. FILTER_FN (optional) gets the
# matched text and returns 0 when the match is allowed.
scan() {
  local rule=$1 re=$2 flags=${3:-} filter=${4:-} f hit line m last
  local -a files
  mapfile -t files < <(git ls-files | grep -vxE 'LICENSE|scripts/pii-lint\.(sh|ps1)')
  for f in "${files[@]}"; do
    last=""
    while IFS= read -r hit; do
      line=${hit%%:*}
      m=${hit#*:}
      [ "$line" = "$last" ] && continue
      if [ -n "$filter" ] && "$filter" "$m"; then
        continue
      fi
      printf '%s:%s: %s\n' "$f" "$line" "$rule"
      last=$line
      fail=1
    done < <(grep -IonE $flags -- "$re" "$f")
  done
}

# Windows or POSIX user-profile paths (case-insensitive drive and folder names).
scan "user-profile path" \
  '([A-Za-z]:[\/]+Users[\/]+|/Users/|/home/)[^\/[:space:]"'"'"'`<>]*' \
  -i is_allowed_user

# Email addresses outside the allowed set.
scan "email address" \
  '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' \
  -i is_allowed_email

# Provider token prefixes with a plausible body length.
scan "secret-looking token" \
  '(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|(^|[^A-Za-z0-9])sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}' \
  ""

# RFC 1918 private IPv4 ranges (127.0.0.1 is loopback and allowed).
scan "private IP address" \
  '(^|[^0-9.])(10\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|192\.168\.[0-9]{1,3}\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3})([^0-9.]|$)' \
  "" ""

if [ "$fail" -ne 0 ]; then
  echo "pii-lint: findings above. Use placeholders (<home>, example) and remove secrets." >&2
  exit 1
fi
echo "pii-lint: clean"
