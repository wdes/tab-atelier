#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Assert that `tab-atelier fleet --json` is renderable as a graph.

Checks the contract a viewer depends on: every edge points at a node that
exists, the three node kinds are present, and each `works_on` edge says
whether a live lease still backs it.
"""
import json
import sys

path = sys.argv[1] if len(sys.argv) > 1 else "-"
raw = sys.stdin.read() if path == "-" else open(path).read()
g = json.loads(raw)

nodes = {n["id"]: n for n in g["nodes"]}
problems = []

if len(nodes) != len(g["nodes"]):
    problems.append("duplicate node ids")

kinds = {n["kind"] for n in g["nodes"]}
for want in ("host", "agent", "task"):
    if want not in kinds:
        problems.append(f"no {want} nodes")

# A dangling edge is the classic way a graph render blows up.
for e in g["edges"]:
    for side in ("from", "to"):
        if e[side] not in nodes:
            problems.append(f"edge {e['kind']} points at missing node {e[side]}")

works = [e for e in g["edges"] if e["kind"] == "works_on"]
if not works:
    problems.append("no works_on edges — nobody appears to be working")
for e in works:
    # Absent means "no lease", which a renderer must be able to tell from a
    # live one; the field is serialised whenever an award exists.
    if "leased" not in e:
        problems.append(f"works_on {e['from']}->{e['to']} carries no lease state")

hosts = sorted(n["label"] for n in g["nodes"] if n["kind"] == "host")
print(
    f"     nodes={len(g['nodes'])} edges={len(g['edges'])} working={len(works)} hosts={hosts}",
    file=sys.stderr,
)

if problems:
    for p in problems:
        print(f"     ! {p}", file=sys.stderr)
    sys.exit(1)
