#!/usr/bin/env bash
# Falla si queda marca de upstream fuera de los sitios donde se permite a propósito:
# la atribución (NOTICE, THIRD_PARTY, CHANGES, README, AGENTS), los nombres de
# crate `goose*` e identificadores (se conservan para poder mergear upstream), el
# namespace ACP `_goose/unstable/*` y las claves de protocolo `"goose"`,
# `goose.local`, y los archivos `.goose*` (compat con clientes y proyectos de goose).
# Sólo caza la marca VISIBLE: prosa, títulos, nombres de producto.
set -euo pipefail
cd "$(dirname "$0")/.."
hits=$(grep -rniE 'goose|block, inc|aaif|posthog|tetrate' \
  --include='*.rs' --include='*.md' --include='*.toml' --include='*.yaml' --include='*.yml' --include='*.json' \
  --exclude-dir=target --exclude-dir=.git . \
  | grep -vE '^\./(NOTICE|THIRD_PARTY_NOTICES\.md|CHANGES-FROM-UPSTREAM\.md|README\.md|CONTRIBUTING\.md|AGENTS\.md|Cargo\.lock)' \
  | grep -vE '/tests?/|tests\.rs:|acp-schema\.json|acp-meta\.json' \
  | grep -vE '_goose/|goose::|[A-Za-z_]*goose_[a-z_]*|_goose\b|Goose[A-Z][A-Za-z]*|goose-[a-z0-9]|\.goose|goose\.|"goose"|goose/|name = "goose|path = "\.\./goose|goose = \{' \
  || true)
if [ -n "$hits" ]; then
  echo "Marca de upstream fuera de la atribución:" >&2
  echo "$hits" >&2
  exit 1
fi
echo "sin marca residual"
