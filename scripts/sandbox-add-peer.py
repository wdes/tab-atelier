#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Register a peer endpoint in a sandbox preferences.json (no CLI needed)."""
import json, sys, uuid
path, label, port, token = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
doc = json.load(open(path))
eps = doc.setdefault("remote_endpoints", [])
if not any(e.get("label") == label for e in eps):
    eps.append({
        "id": str(uuid.uuid4()), "label": label, "url": f"http://127.0.0.1:{port}",
        "token": token, "cert_sha256": "", "autoconnect": False,
    })
json.dump(doc, open(path, "w"))
