#!/usr/bin/env bash
# Falla si queda marca de upstream fuera de los sitios donde se permite a propósito:
# la atribución (NOTICE, THIRD_PARTY, CHANGES), los nombres de crate `goose*`
# (internos, se conservan para poder mergear upstream) y el namespace ACP
# `_goose/unstable/*` (compat con clientes existentes).
set -euo pipefail
cd "$(dirname "$0")/.."
hits=$(grep -rniE 'goose|block, inc|aaif|posthog|tetrate' \
  --include='*.rs' --include='*.md' --include='*.toml' --include='*.yaml' --include='*.yml' --include='*.json' \
  --exclude-dir=target --exclude-dir=.git . \
  | grep -vE '^\./(NOTICE|THIRD_PARTY_NOTICES\.md|CHANGES-FROM-UPSTREAM\.md|Cargo\.lock)' \
  | grep -vE '_goose/|goose_mode|use goose|goose::|goose_[a-z_]+::|goose-(cli|mcp|providers|provider-types|sdk-types|acp-macros|agent|context-management|test|test-support)|crates/goose|name = "goose|path = "\.\./goose|goose_[a-z_]+ = \{|goose = \{' \
  || true)
if [ -n "$hits" ]; then
  echo "Marca de upstream fuera de la atribución:" >&2
  echo "$hits" >&2
  exit 1
fi
echo "sin marca residual"
