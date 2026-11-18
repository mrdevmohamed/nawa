#!/usr/bin/env python3
"""Fail when the crate's module structure regresses.

Checks (production code only; `#[cfg(test)]` items are ignored):

  1. no dependency cycle between top-level modules (`crate::a` -> `crate::b`
     -> `crate::a`, however long the chain);
  2. no `use crate::*` glob import, which hides the real dependencies;
  3. dependency direction: modules outside the presentation layer must not
     import presentation modules (`cli`, `commands`, `output`, `color`,
     `diag`, `humanize`, `confirm`, `bootstrap`, `main`), except for the
     explicitly allowlisted legacy edges below; use-case code under
     `application/features/` may never import presentation, allowlist or
     not. Stale allowlist entries (edges that no longer exist) also fail:
     the list must shrink as use cases are extracted.

Run from the repository root:  python3 scripts/check-layering.py
Standard library only.
"""
import collections
import os
import re
import sys

SRC = "src"

# Presentation renders, prompts and formats; everything else (application
# policy, use cases, domain logic, adapters in transition) must not import
# it. Presentation itself may import anything — rendering is the outermost
# layer.
PRESENTATION = frozenset({
    "bootstrap",
    "cli",
    "color",
    "commands",
    "confirm",
    "diag",
    "humanize",
    "main",
    "output",
})

# Legacy adapter -> presentation edges that predate the feature-oriented
# split. Every entry is waiting on a specific extraction documented in
# docs/implementation-progress.md; removing the edge removes the entry.
# Entries must shrink: an edge that no longer exists fails the check.
LEGACY_PRESENTATION_IMPORTS = {
    # undo prompts inside the compensation flow; moves behind the user
    # interaction port with the history use cases
    ("undo", "confirm"),
    # fetch/fetchrun/install/plugins/repositories/slim/upgrade print progress
    # notes and format sizes inline; those notes move out with the build,
    # repository and plugin use-case extractions
    ("fetch", "diag"),
    ("fetch", "humanize"),
    ("fetchrun", "diag"),
    ("install", "diag"),
    ("plugins", "diag"),
    ("repositories", "humanize"),
    ("slim", "humanize"),
    ("upgrade", "color"),
    ("upgrade", "confirm"),
    ("upgrade", "diag"),
}


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

    # use-case code under application/features/ is the new functional core:
    # it never imports presentation, allowlist or not
    def features_file(p):
        return os.path.relpath(p, SRC).startswith("application/features/")

    for m in sorted(names):
        for t in sorted(edges[m]):
            if t not in PRESENTATION:
                continue
            for p in sorted(edges[m][t]):
                if features_file(p):
                    failed = True
                    print(f"error: use-case code must not import presentation {t}:")
                    print(f"    {p}")

    # direction rules: nothing outside presentation imports presentation code
    def presentation_edge(m, t):
        return t in PRESENTATION and m not in PRESENTATION and (m, t) not in LEGACY_PRESENTATION_IMPORTS

    for m in sorted(names):
        for t in sorted(edges[m]):
            if not presentation_edge(m, t):
                continue
            general = [p for p in sorted(edges[m][t]) if not features_file(p)]
            if not general:
                continue  # covered by the application/features rule above
            failed = True
            print(
                f"error: {m} imports presentation {t} — decide, render nothing:"
                " move the message into the return value and print it in the handler,"
                " or extend the documented allowlist if it is a legacy edge"
            )
            for p in general:
                print(f"    {p}")

    # The migration gate-crossing capability is minted only by the migration
    # feature subtree. A call from any other module would let an ordinary
    # package command cross a pending migration's gate (§10.3/§13).
    MIGRATION_MINT = "migration_authority()"
    MIGRATION_MINT_ALLOWED = "application/features/system/"
    for m, paths in modules.items():
        for p in paths:
            for n, line in enumerate(strip_tests(open(p).read()), 1):
                if MIGRATION_MINT in line:
                    rel = os.path.relpath(p, SRC).replace(os.sep, "/")
                    if rel != "lock.rs" and not rel.startswith(MIGRATION_MINT_ALLOWED):
                        failed = True
                        print(
                            "error: only migration feature code may mint the "
                            f"migration gate-crossing capability: {p}:{n}"
                        )

    # the allowlist documents temporary exceptions and must shrink
    for entry in sorted(LEGACY_PRESENTATION_IMPORTS):
        if not edges[entry[0]].get(entry[1]):
            failed = True
            print(
                f"error: stale layering allowlist entry ({entry[0]}, {entry[1]}):"
                " the edge is gone — remove the exception"
            )

    if failed:
        return 1
    print(f"layering ok: {len(names)} modules, no cycles, no glob imports of the crate root")
    return 0


if __name__ == "__main__":
    sys.exit(main())
