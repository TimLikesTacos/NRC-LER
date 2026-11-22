#!/usr/bin/env python3
"""NRC ADAMS Public Search (APS) API client -> SQLite.

Get a key: https://adams-api-developer.nrc.gov/ -> Sign up -> Products ->
"ADAMS Public Search API" -> Subscribe.  Then:  export NRC_APS_KEY=...

Commands:
  probe                          find which DocumentType / search terms hold event reports
  search  [--type T] [--q Q]     print one page of results
  ingest  --type T [--q Q] [--since YYYY-MM-DD] [--until YYYY-MM-DD]
                                 page through results, store metadata + full text
  get     ACCESSION              print one document
"""
import argparse
import datetime as dt
import json
import os
import sqlite3
import sys
import time
import urllib.error
import urllib.request

API = "https://adams-api.nrc.gov/aps/api/search"

SCHEMA = """
CREATE TABLE IF NOT EXISTS adams_documents (
    accession_number    TEXT PRIMARY KEY,
    document_title      TEXT,
    document_type       TEXT,   -- JSON array
    document_date       TEXT,
    date_added          TEXT,
    docket_number       TEXT,   -- JSON array
    report_number       TEXT,   -- JSON array
    author_affiliation  TEXT,   -- JSON array
    url                 TEXT,
    content             TEXT,   -- full indexed text (from GET by accession)
    metadata            TEXT,   -- full JSON document object
    fetched_at          TEXT
);
CREATE INDEX IF NOT EXISTS idx_adams_date ON adams_documents(document_date);
"""

# Candidate ways Event Notification Reports might be catalogued in ADAMS.
PROBES = [
    {"type": "Event Notification Report"},
    {"type": "Event Notification", "op": "starts"},
    {"type": "Event Report", "op": "contains"},
    {"type": "Licensee Event Report"},
    {"type": "Part 21 Correspondence"},
    {"q": "\"Event Notification Report\""},
    {"q": "\"Operations Center\" \"Event Number\""},
]


def key():
    k = os.environ.get("NRC_APS_KEY")
    if not k:
        sys.exit("Set NRC_APS_KEY (see https://adams-api-developer.nrc.gov/).")
    return k


def call(method, url, body=None, retries=4):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers={
        "Ocp-Apim-Subscription-Key": key(),
        "Content-Type": "application/json",
        "Accept": "application/json",
        "Cache-Control": "no-cache",
    })
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                return json.loads(r.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            msg = e.read().decode("utf-8", "replace")[:500]
            if e.code == 429 or e.code >= 500:
                wait = int(e.headers.get("Retry-After") or 10 * (attempt + 1))
                print("  HTTP %d, retrying in %ds" % (e.code, wait), file=sys.stderr)
                time.sleep(wait)
                continue
            sys.exit("HTTP %d from %s\n%s" % (e.code, url, msg))
        except (urllib.error.URLError, TimeoutError) as e:
            print("  %s, retrying" % e, file=sys.stderr)
            time.sleep(10 * (attempt + 1))
    sys.exit("giving up on %s" % url)


def build_query(q="", doc_type=None, op="equals", since=None, until=None, skip=0, legacy=False):
    filters = []
    if doc_type:
        filters.append({"field": "DocumentType", "value": doc_type, "operator": op})
    if since:
        filters.append({"field": "DocumentDate", "value": "(DocumentDate ge '%s')" % since})
    if until:
        filters.append({"field": "DocumentDate", "value": "(DocumentDate le '%s')" % until})
    return {"q": q or "", "filters": filters, "anyFilters": [],
            "mainLibFilter": not legacy, "legacyLibFilter": legacy,
            "sort": "DocumentDate", "sortDirection": 0, "skip": skip}


def search(**kw):
    return call("POST", API, build_query(**kw))


def get_doc(accession):
    return call("GET", "%s/%s" % (API, accession)).get("document", {})


def summarize(res, n=5):
    docs = [r["document"] for r in res.get("results", [])]
    print("  count=%s  page_size=%d" % (res.get("count"), len(docs)))
    for d in docs[:n]:
        print("   %s  %s  %s  %s" % (d.get("AccessionNumber"), d.get("DocumentDate"),
                                   d.get("DocumentType"), (d.get("DocumentTitle") or "")[:90]))


def cmd_probe(a):
    for p in PROBES:
        print("PROBE", p)
        summarize(search(q=p.get("q", ""), doc_type=p.get("type"), op=p.get("op", "equals")))
        time.sleep(a.delay)


def store(con, d, content):
    j = lambda v: json.dumps(v) if v is not None else None
    con.execute("""INSERT OR REPLACE INTO adams_documents VALUES (?,?,?,?,?,?,?,?,?,?,?,?)""", (
        d.get("AccessionNumber"), d.get("DocumentTitle"), j(d.get("DocumentType")),
        d.get("DocumentDate"), d.get("DateAddedTimestamp") or d.get("DateAdded"),
        j(d.get("DocketNumber")), j(d.get("DocumentReportNumber")), j(d.get("AuthorAffiliation")),
        d.get("Url"), content, json.dumps(d), dt.datetime.now().isoformat(timespec="seconds")))


def cmd_ingest(a):
    con = sqlite3.connect(a.db)
    con.executescript(SCHEMA)
    have = {r[0] for r in con.execute("SELECT accession_number FROM adams_documents WHERE content IS NOT NULL")}
    skip, total = 0, None
    while True:
        res = search(q=a.q, doc_type=a.type, op=a.op, since=a.since, until=a.until, skip=skip, legacy=a.legacy)
        docs = [r["document"] for r in res.get("results", [])]
        total = res.get("count", total)
        if not docs:
            break
        for d in docs:
            acc = d.get("AccessionNumber")
            if acc in have:
                continue
            # Search results carry only a snippet of content; GET returns the full text.
            full = get_doc(acc) if a.full_text else {}
            store(con, {**d, **{k: v for k, v in full.items() if k != "content"}},
                  full.get("content") if a.full_text else None)
            have.add(acc)
            time.sleep(a.delay)
        con.commit()
        skip += len(docs)
        print("  %d / %s" % (skip, total))
        if total is not None and skip >= int(total):
            break
        time.sleep(a.delay)
    print("done: %d documents in db" % con.execute("SELECT COUNT(*) FROM adams_documents").fetchone()[0])


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["probe", "search", "ingest", "get"])
    ap.add_argument("accession", nargs="?")
    ap.add_argument("--q", default="")
    ap.add_argument("--type")
    ap.add_argument("--op", default="equals", choices=["equals", "contains", "starts", "notcontains", "notstarts", "notequals"])
    ap.add_argument("--since")
    ap.add_argument("--until")
    ap.add_argument("--db", default="nrc_events.db")
    ap.add_argument("--legacy", action="store_true", help="search the pre-1999 legacy library instead of the main one")
    ap.add_argument("--delay", type=float, default=0.5)
    ap.add_argument("--no-full-text", dest="full_text", action="store_false")
    a = ap.parse_args()
    if a.cmd == "probe":
        cmd_probe(a)
    elif a.cmd == "search":
        summarize(search(q=a.q, doc_type=a.type, op=a.op, since=a.since, until=a.until, legacy=a.legacy), n=25)
    elif a.cmd == "get":
        d = get_doc(a.accession)
        c = d.pop("content", "") or ""
        print(json.dumps(d, indent=2))
        print("--- content (%d chars) ---\n%s" % (len(c), c[:3000]))
    else:
        if not (a.type or a.q):
            sys.exit("ingest needs --type and/or --q")
        cmd_ingest(a)


if __name__ == "__main__":
    main()
