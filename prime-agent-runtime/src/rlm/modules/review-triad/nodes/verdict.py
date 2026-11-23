"""Merge the three reviews: approve only when every reviewer approves.

Findings from the rejecting reviewers are split into items, deduplicated across
reviewers by a hash of their normalized text, and formatted one per line as
`[id] severity category file:line - message (lenses)`.
"""

import hashlib
import re

LENSES = ("correctness", "bugs", "robustness")
SEVERITIES = {"critical": "high", "high": "high", "major": "high", "medium": "med",
              "med": "med", "moderate": "med", "low": "low", "minor": "low", "nit": "low"}
_BULLET = re.compile(r"^\s*(?:[-*\u2022]|\d+[.)])\s+")
_SEVERITY = re.compile(r"^\s*[\[(]?\s*(critical|high|major|medium|med|moderate|low|minor|nit)\s*[\])]?\s*[:\-\u2013]?\s*", re.I)
_CATEGORY = re.compile(r"^\s*[\[(]?\s*([a-z][a-z_-]{1,24})\s*[\])]\s*[:\-\u2013]?\s*", re.I)
_LOCATION = re.compile(r"`?([\w./-]+\.[A-Za-z0-9]+)(?::(\d+))?`?")


def _items(text):
    """Split one reviewer's findings into items: bullets, or blank-line paragraphs."""
    items, current = [], []
    for line in text.splitlines():
        if _BULLET.match(line) or not line.strip():
            if current:
                items.append(" ".join(current))
            current = [_BULLET.sub("", line).strip()] if line.strip() else []
        else:
            current.append(line.strip())
    if current:
        items.append(" ".join(current))
    return [item for item in items if item]


def _parse(item, lens):
    severity, category = "med", lens
    match = _SEVERITY.match(item)
    if match:
        severity = SEVERITIES[match.group(1).lower()]
        item = item[match.end():]
    match = _CATEGORY.match(item)
    if match:
        category = match.group(1).lower()
        item = item[match.end():]
    location = _LOCATION.search(item)
    file = location.group(1) if location else ""
    line = int(location.group(2)) if location and location.group(2) else None
    message = " ".join(item.split())
    key = re.sub(r"[^a-z0-9]+", " ", message.lower()).strip()
    digest = hashlib.sha1(f"{file}\0{key}".encode()).hexdigest()[:10]
    return {"id": digest, "file": file, "line": line, "severity": severity,
            "category": category, "message": message, "lenses": [lens]}


def _format(finding):
    where = finding["file"] + (f":{finding['line']}" if finding["line"] else "")
    head = " ".join(p for p in (finding["severity"], finding["category"], where) if p)
    return f"[{finding['id']}] {head} - {finding['message']} ({', '.join(finding['lenses'])})"


def merge(inputs):
    approve = all(inputs.get(lens) is True for lens in LENSES)
    merged = {}
    for lens in LENSES:
        text = (inputs.get(lens + "_findings") or "").strip()
        if not text or inputs.get(lens) is True:
            continue
        for item in _items(text):
            finding = _parse(item, lens)
            seen = merged.get(finding["id"])
            if seen is None:
                merged[finding["id"]] = finding
            elif lens not in seen["lenses"]:
                seen["lenses"].append(lens)
    rank = {"high": 0, "med": 1, "low": 2}
    ordered = sorted(merged.values(), key=lambda f: (rank[f["severity"]], f["file"], f["line"] or 0))
    return {"verdict": "approve" if approve else "rework",
            "findings": "\n".join(_format(f) for f in ordered)}
