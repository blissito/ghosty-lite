#!/usr/bin/env python3
"""Quita de archivos Rust todo lo protegido por `#[cfg(feature = "X")]`.

- `#[cfg(feature = "X")]` + el ítem siguiente → se borran los dos.
- `#[cfg(not(feature = "X"))]` → se borra sólo el atributo (el ítem se queda).
- `#[cfg(any(feature = "X", ...))]` / `all(...)` → se deja intacto y se avisa.

Uso: strip_feature.py FEATURE archivo.rs [...]
"""
import re, sys

feature = sys.argv[1]
GATE = re.compile(r'^\s*#\[cfg\(feature\s*=\s*"%s"\)\]\s*$' % re.escape(feature))
NOT_GATE = re.compile(r'^\s*#\[cfg\(not\(feature\s*=\s*"%s"\)\)\]\s*$' % re.escape(feature))
COMPLEX = re.compile(r'#\[cfg\((any|all)\(.*feature\s*=\s*"%s"' % re.escape(feature))
SEMI_START = re.compile(r'^\s*(pub(\([^)]*\))?\s+)?(let|use|mod|type|static|const|extern crate)\b')

def arm_arrow(line):
    """Posición de un `=>` a profundidad 0 (brazo de match), o None."""
    depth = 0; in_str = False; j = 0
    while j < len(line):
        c = line[j]
        if in_str:
            if c == '\\': j += 1
            elif c == '"': in_str = False
        elif c == '"': in_str = True
        elif line.startswith('//', j): return None
        elif c in '{([': depth += 1
        elif c in '})]': depth -= 1
        elif depth == 0 and line.startswith('=>', j): return j
        j += 1
    return None

def item_end(lines, i):
    """Índice (exclusivo) de la última línea del ítem que empieza en lines[i]."""
    # atributos adicionales (#[command(...)] pueden abarcar varias líneas)
    while lines[i].lstrip().startswith('#['):
        depth = 0
        while True:
            depth += lines[i].count('[') - lines[i].count(']')
            i += 1
            if depth <= 0:
                break
    start = i
    first = lines[i]
    col = 0  # columna desde la que se cuentan los delimitadores en la primera línea
    arrow = arm_arrow(first)
    if SEMI_START.match(first):
        term = ';'
    elif arrow is not None:
        # brazo de match: lo que hay antes de `=>` (patrón) no cuenta
        col = arrow + 2
        rest = first[col:].strip()
        term = None if rest.startswith('{') else ','
    else:
        term = None  # bloque
    depth = 0
    in_str = False
    seen_block = False
    while i < len(lines):
        line = lines[i]
        j = col if i == start else 0
        while j < len(line):
            c = line[j]
            if in_str:
                if c == '\\': j += 1
                elif c == '"': in_str = False
            elif c == '"': in_str = True
            elif line.startswith('//', j): break
            elif c in '{([': depth += 1; seen_block = seen_block or c == '{'
            elif c in '})]':
                depth -= 1
                if depth == 0 and c == '}' and term is None:
                    # cierre del bloque; consume coma final si la hay
                    rest = line[j+1:].strip()
                    return i + 1
            elif depth == 0 and term and c == term:
                return i + 1
            j += 1
        if term is None and depth == 0 and not seen_block:
            return i + 1  # ítem de una línea sin bloque (campo, variante unitaria)
        i += 1
    return i

for path in sys.argv[2:]:
    src = open(path).read().split('\n')
    out, i, removed = [], 0, 0
    while i < len(src):
        line = src[i]
        if GATE.match(line):
            # doc comments justo encima del gate documentan al ítem que se va
            while out and out[-1].lstrip().startswith('///'):
                out.pop()
            end = item_end(src, i + 1)
            i = end
            removed += 1
            continue
        if NOT_GATE.match(line):
            i += 1
            removed += 1
            continue
        if COMPLEX.search(line):
            print(f"AVISO {path}:{i+1}: cfg compuesto, revisar a mano: {line.strip()}", file=sys.stderr)
        out.append(line)
        i += 1
    if removed:
        open(path, 'w').write('\n'.join(out))
        print(f"{path}: {removed} gates")
