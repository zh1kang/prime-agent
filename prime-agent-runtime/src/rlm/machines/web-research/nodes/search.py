"""Exa-inspired parallel web & academic retrieval with highlight extraction.

When ``EXA_API_KEY`` is set, queries fan out concurrently against Exa's neural
``/search`` endpoint requesting compact semantic highlights (avoiding full-page
HTML bloat). When ``EXA_API_KEY`` is absent, falls back cleanly to zero-key
open APIs (arXiv Atom, GitHub Search, Wikipedia) and instructs downstream
investigation agents to augment with their native web-search tools.
"""

from __future__ import annotations

import concurrent.futures
import json
import os
import re
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from typing import Any

MAX_QUERIES = 6
MAX_WORKERS = 4
HTTP_TIMEOUT_S = 15.0
MAX_EVIDENCE_CHARS = 14_000
MAX_SNIPPET_CHARS = 900
_SITE_RE = re.compile(r"\bsite:([A-Za-z0-9.-]+)\b", re.IGNORECASE)
_CODE_HINTS = ("github", "repo", "code", "library", "implementation", "benchmark", "sdk", "crate", "package")
_PAPER_HINTS = (
    "paper", "arxiv", "theorem", "proof", "benchmark", "dataset", "neural",
    "plasticity", "transformer", "architecture", "algorithm", "study", "sota", "method",
)
_USER_AGENT = "radiata-web-research/1.0 (+https://github.com/zh1kang/prime-agent)"


def _parse_queries(raw: Any, fallback_query: str = "") -> list[str]:
    """Normalize planner query output into a bounded deduplicated list."""
    items: list[str] = []
    if isinstance(raw, list):
        items = [str(x).strip() for x in raw if isinstance(x, (str, int, float)) and str(x).strip()]
    elif isinstance(raw, dict):
        nested = raw.get("queries") or raw.get("subqueries") or []
        if isinstance(nested, list):
            items = [str(x).strip() for x in nested if str(x).strip()]
    elif isinstance(raw, str) and raw.strip():
        text = raw.strip()
        try:
            parsed = json.loads(text)
            return _parse_queries(parsed, fallback_query)
        except ValueError:
            items = [
                re.sub(r"^\s*(?:[-*\u2022]|\d+[.)])\s*", "", line).strip()
                for line in text.splitlines()
                if line.strip()
            ]
    if not items and fallback_query.strip():
        items = [fallback_query.strip()]
    seen: set[str] = set()
    out: list[str] = []
    for q in items:
        key = " ".join(q.lower().split())
        if key and key not in seen:
            seen.add(key)
            out.append(" ".join(q.split())[:260])
        if len(out) >= MAX_QUERIES:
            break
    return out


def _canonical_url(url: str) -> str:
    """Normalize a URL for cross-query deduplication."""
    raw = (url or "").strip()
    if not raw:
        return ""
    parsed = urllib.parse.urlsplit(raw)
    scheme = (parsed.scheme or "https").lower()
    netloc = parsed.netloc.lower()
    if netloc.startswith("www."):
        netloc = netloc[4:]
    path = parsed.path.rstrip("/") or "/"
    params = [
        (k, v)
        for k, v in urllib.parse.parse_qsl(parsed.query, keep_blank_values=False)
        if not k.lower().startswith("utm_") and k.lower() not in {"ref", "source", "fbclid", "gclid"}
    ]
    query = urllib.parse.urlencode(sorted(params))
    return urllib.parse.urlunsplit((scheme, netloc, path, query, ""))


def _extract_domains(query: str) -> tuple[str, list[str]]:
    domains = _SITE_RE.findall(query)
    cleaned = " ".join(_SITE_RE.sub("", query).split())
    return cleaned or query, sorted({d.lower() for d in domains})


def _http_get(url: str, *, headers: dict[str, str] | None = None) -> bytes:
    req = urllib.request.Request(
        url,
        headers={"User-Agent": _USER_AGENT, **(headers or {})},
        method="GET",
    )
    with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as resp:
        return resp.read()


def _exa_search(query: str, api_key: str) -> list[dict[str, Any]]:
    """Run one Exa neural/auto search with semantic highlights."""
    cleaned, domains = _extract_domains(query)
    payload: dict[str, Any] = {
        "query": cleaned,
        "type": "auto",
        "numResults": 5,
        "contents": {
            "highlights": {
                "numSentences": 4,
                "highlightsPerUrl": 3,
                "query": cleaned,
            }
        },
    }
    if domains:
        payload["includeDomains"] = domains
    body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        "https://api.exa.ai/search",
        data=body,
        headers={
            "x-api-key": api_key,
            "Content-Type": "application/json",
            "Accept": "application/json",
            "User-Agent": _USER_AGENT,
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as resp:
        data = json.loads(resp.read().decode("utf-8", errors="replace"))
    hits: list[dict[str, Any]] = []
    for item in data.get("results") or []:
        if not isinstance(item, dict):
            continue
        url = str(item.get("url") or "").strip()
        if not url:
            continue
        highlights = [
            " ".join(str(h).split())
            for h in (item.get("highlights") or [])
            if isinstance(h, str) and h.strip()
        ]
        snippet = " ... ".join(highlights) if highlights else " ".join(str(item.get("text") or "").split())
        hits.append(
            {
                "query": query,
                "source": "exa",
                "title": " ".join(str(item.get("title") or url).split())[:180],
                "url": url,
                "date": str(item.get("publishedDate") or "")[:10],
                "snippet": snippet[:MAX_SNIPPET_CHARS],
            }
        )
    return hits


def _arxiv_search(query: str) -> list[dict[str, Any]]:
    cleaned, _ = _extract_domains(query)
    encoded = urllib.parse.quote(cleaned)
    url = f"https://export.arxiv.org/api/query?search_query=all:{encoded}&start=0&max_results=4"
    raw = _http_get(url)
    root = ET.fromstring(raw)
    ns = {"a": "http://www.w3.org/2005/Atom"}
    hits: list[dict[str, Any]] = []
    for entry in root.findall("a:entry", ns):
        entry_url = (entry.findtext("a:id", default="", namespaces=ns) or "").strip()
        title = " ".join((entry.findtext("a:title", default="", namespaces=ns) or "").split())
        summary = " ".join((entry.findtext("a:summary", default="", namespaces=ns) or "").split())
        published = (entry.findtext("a:published", default="", namespaces=ns) or "")[:10]
        if entry_url and title:
            hits.append(
                {
                    "query": query,
                    "source": "arxiv",
                    "title": title[:180],
                    "url": entry_url,
                    "date": published,
                    "snippet": summary[:MAX_SNIPPET_CHARS],
                }
            )
    return hits


def _github_search(query: str) -> list[dict[str, Any]]:
    cleaned, _ = _extract_domains(query)
    encoded = urllib.parse.quote(cleaned)
    url = f"https://api.github.com/search/repositories?q={encoded}&sort=stars&per_page=3"
    headers = {"Accept": "application/vnd.github+json"}
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token.strip()}"
    data = json.loads(_http_get(url, headers=headers).decode("utf-8", errors="replace"))
    hits: list[dict[str, Any]] = []
    for repo in data.get("items") or []:
        if not isinstance(repo, dict):
            continue
        html_url = str(repo.get("html_url") or "").strip()
        if not html_url:
            continue
        desc = " ".join(str(repo.get("description") or "").split())
        stars = repo.get("stargazers_count", 0)
        lang = repo.get("language") or ""
        updated = str(repo.get("pushed_at") or "")[:10]
        hits.append(
            {
                "query": query,
                "source": "github",
                "title": f"{repo.get('full_name', html_url)} ({stars} stars{', ' + lang if lang else ''})",
                "url": html_url,
                "date": updated,
                "snippet": desc[:MAX_SNIPPET_CHARS],
            }
        )
    return hits


def _wikipedia_search(query: str) -> list[dict[str, Any]]:
    cleaned, _ = _extract_domains(query)
    encoded = urllib.parse.quote(cleaned)
    url = (
        "https://en.wikipedia.org/w/api.php?action=query&list=search&utf8=1"
        f"&format=json&srlimit=2&srsearch={encoded}"
    )
    data = json.loads(_http_get(url).decode("utf-8", errors="replace"))
    hits: list[dict[str, Any]] = []
    for item in ((data.get("query") or {}).get("search") or []):
        title = str(item.get("title") or "").strip()
        if not title:
            continue
        snippet = re.sub(r"<[^>]+>", "", str(item.get("snippet") or ""))
        slug = urllib.parse.quote(title.replace(" ", "_"))
        hits.append(
            {
                "query": query,
                "source": "wikipedia",
                "title": title[:180],
                "url": f"https://en.wikipedia.org/wiki/{slug}",
                "date": str(item.get("timestamp") or "")[:10],
                "snippet": " ".join(snippet.split())[:MAX_SNIPPET_CHARS],
            }
        )
    return hits


def _open_api_search(query: str) -> list[dict[str, Any]]:
    lowered = query.lower()
    hits: list[dict[str, Any]] = []
    tasks = []
    if any(tok in lowered for tok in _PAPER_HINTS) or "arxiv" in lowered:
        tasks.append(_arxiv_search)
    if any(tok in lowered for tok in _CODE_HINTS):
        tasks.append(_github_search)
    if not tasks:
        tasks.extend([_arxiv_search, _wikipedia_search])
    for fn in tasks:
        try:
            hits.extend(fn(query))
        except Exception:
            continue
    return hits


def _format_evidence(hits: list[dict[str, Any]], backend: str, queries: list[str], errors: list[str]) -> str:
    lines = [
        f"Retrieval backend: {backend}",
        "Executed sub-queries:",
        *[f"- {q}" for q in queries],
    ]
    if errors:
        lines.append(f"Retrieval notes: {'; '.join(errors[:3])}")
    if not hits:
        lines.append(
            "\nNo pre-fetched snippets returned. Use your native web search and URL fetch tools "
            "to investigate each sub-query directly."
        )
        return "\n".join(lines)
    lines.append(f"\nDeduplicated highlights ({len(hits)} unique sources):")
    total = sum(len(line) + 1 for line in lines)
    for idx, hit in enumerate(hits, start=1):
        date_part = f" ({hit['date']})" if hit.get("date") else ""
        block = (
            f"\n[{idx}] {hit['title']}{date_part} [{hit['source']}]\n"
            f"URL: {hit['url']}\n"
            f"Query: {hit['query']}\n"
            f"Highlights: {hit['snippet'] or '(no excerpt)'}"
        )
        if total + len(block) > MAX_EVIDENCE_CHARS:
            lines.append(f"\n... ({len(hits) - idx + 1} more sources omitted to stay within budget)")
            break
        lines.append(block)
        total += len(block)
    return "\n".join(lines)


def retrieve(inputs: dict[str, Any]) -> dict[str, Any]:
    """Fan out sub-queries via Exa Highlights (if EXA_API_KEY is set) or open APIs."""
    fallback_query = str(inputs.get("query") or inputs.get("topic") or "")
    queries = _parse_queries(inputs.get("queries"), fallback_query=fallback_query)
    prior_raw = inputs.get("prior_urls")
    prior_seen: set[str] = set()
    if isinstance(prior_raw, list):
        prior_seen = {_canonical_url(str(u)) for u in prior_raw if str(u).strip()}

    exa_key = (os.environ.get("EXA_API_KEY") or "").strip()
    backend = "exa" if exa_key else "open_api_fallback"
    errors: list[str] = []
    raw_hits: list[dict[str, Any]] = []

    def _run_one(q: str) -> list[dict[str, Any]]:
        if exa_key:
            try:
                return _exa_search(q, exa_key)
            except Exception as err:
                errors.append(f"Exa failed for {q[:40]!r}: {err}")
                return _open_api_search(q)
        return _open_api_search(q)

    with concurrent.futures.ThreadPoolExecutor(max_workers=min(MAX_WORKERS, max(1, len(queries)))) as pool:
        for batch in pool.map(_run_one, queries):
            raw_hits.extend(batch)

    deduped: list[dict[str, Any]] = []
    seen = set(prior_seen)
    urls: list[str] = list(prior_raw) if isinstance(prior_raw, list) else []
    for hit in raw_hits:
        canon = _canonical_url(hit.get("url", ""))
        if not canon or canon in seen:
            continue
        seen.add(canon)
        deduped.append(hit)
        urls.append(hit["url"])

    return {
        "evidence": _format_evidence(deduped, backend, queries, errors),
        "urls": urls,
        "backend": backend,
    }
