#!/usr/bin/env python3
"""Fail when the crate's module structure regresses.

Checks (production code only; `#[cfg(test)]` items are ignored):

  1. no dependency cycle between top-level modules (`crate::a` -> `crate::b`
     -> `crate::a`, however long the chain);
  2. no `use crate::*` glob import, which hides the real dependencies.

Run from the repository root:  python3 scripts/check-layering.py
Standard library only.
"""
import collections
import os
import re
import sys

SRC = "src"


def strip_tests(text):
    """Drop `#[cfg(test)]` items (modules, fns, impls) from the source lines."""
    lines, out, i = text.split("\n"), [], 0
    while i < len(lines):
        if lines[i].strip().startswith("#[cfg(test)]"):
            j = i + 1
            while j < len(lines) and lines[j].strip().startswith("#["):
                j += 1
            depth, started, k = 0, False, j
            while k < len(lines):
                depth += lines[k].count("{") - lines[k].count("}")
                started = started or "{" in lines[k]
                if (started and depth <= 0) or (not started and lines[k].rstrip().endswith(";")):
                    break
                k += 1
            i = k + 1
            continue
        out.append(lines[i])
        i += 1
    return out


def top_level(path):
    parts = os.path.relpath(path, SRC)[:-3].split(os.sep)
    if parts[-1] == "mod":
        parts = parts[:-1]
    return parts[0]


def main():
    modules = collections.defaultdict(list)
    for root, _, files in os.walk(SRC):
        for f in files:
            if f.endswith(".rs"):
                p = os.path.join(root, f)
                modules[top_level(p)].append(p)
    names = set(modules)
    edges = {m: collections.defaultdict(set) for m in names}
    globs = []
    for m, paths in modules.items():
        for p in paths:
            for n, line in enumerate(strip_tests(open(p).read()), 1):
                if re.search(r"\buse\s+crate::\*\s*;", line):
                    globs.append(f"{p}: {line.strip()}")
                for hit in re.finditer(r"crate::([a-z_][a-z_0-9]*)", line):
                    t = hit.group(1)
                    if t in names and t != m:
                        edges[m][t].add(p)

    # strongly connected components (Tarjan)
    sys.setrecursionlimit(10000)
    index, low, stack, on, comps, counter = {}, {}, [], set(), [], [0]

    def visit(v):
        index[v] = low[v] = counter[0]
        counter[0] += 1
        stack.append(v)
        on.add(v)
        for w in edges[v]:
            if w not in index:
                visit(w)
                low[v] = min(low[v], low[w])
            elif w in on:
                low[v] = min(low[v], index[w])
        if low[v] == index[v]:
            comp = []
            while True:
                w = stack.pop()
                on.discard(w)
                comp.append(w)
                if w == v:
                    break
            comps.append(sorted(comp))

    for v in sorted(names):
        if v not in index:
            visit(v)

    failed = False
    for comp in comps:
        if len(comp) > 1:
            failed = True
            print(f"error: dependency cycle between modules: {', '.join(comp)}")
            for a in comp:
                for b in sorted(edges[a]):
                    if b in comp:
                        print(f"    {a} -> {b}  ({', '.join(sorted(edges[a][b]))})")
    if globs:
        failed = True
        print("error: `use crate::*` hides module dependencies; import items explicitly:")
        for g in globs:
            print(f"    {g}")
    if failed:
        return 1
    print(f"layering ok: {len(names)} modules, no cycles, no glob imports of the crate root")
    return 0


if __name__ == "__main__":
    sys.exit(main())
