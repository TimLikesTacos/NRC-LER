#!/usr/bin/env python3
"""Pull NRC Accident Sequence Precursor (ASP) documents from ADAMS into asp.db (raw, like lers.db).

The ASP program scores the riskiest operating events: how likely core damage would have been
given what happened (CCDP, for an initiating event such as a trip or loss of offsite power) or how
much a degraded condition raised core damage probability over its exposure time (delta-CDP).

Two kinds of document are kept:
  reports   yearly program results: SECY status papers and their "Results, Trends, and
            Insights" enclosures (2001-2015), then the ASP Program Annual Reports (2016-), each
            with a table of that year's precursors (plant, LER, event date, CCDP/delta-CDP)
  analyses  individual final/preliminary ASP analyses and NRC transmittals of them

ADAMS title search is fuzzy (word matching), so every hit is kept only if its title matches
REPORT or ANALYSIS below. Stored documents are skipped, so a rerun only fetches new ones.

  python3 pull_asp.py [--db asp.db] [--dry-run]
"""
import argparse
import re
import sqlite3
import time

import aps

REPORT = re.compile(
    r"(accident sequence precursor program \d{4} annual report"
    r"|asp program \d{4} annual report"
    r"|status (report )?(of|on) (the )?accident sequence precursor"
    r"|results,? trends,? and insights (from|of) the accident sequence precursor"
    r"|accident sequence precursor program results)", re.I)
ANALYSIS = re.compile(
    r"((final|preliminary)\s+)?(asp|accident sequence precursor|precursor)\s+(screening\s+)?analys[ie]s", re.I)
SKIP = re.compile(r"transmittal of the accident sequence precursor program \d{4} annual", re.I)

TITLE_SEARCHES = [
    "Accident Sequence Precursor Program Annual Report",
    "Status of the Accident Sequence Precursor",
    "Results Trends Insights Accident Sequence Precursor",
    "Precursor Analysis",
    "ASP Analysis",
]


def title_search(title, delay):
    """Every document whose title ADAMS matches to `title` (paged)."""
    skip = 0
    while True:
        body = {"q": "", "filters": [{"field": "DocumentTitle", "value": title, "operator": "contains"}],
                "anyFilters": [], "mainLibFilter": True, "legacyLibFilter": False,
                "sort": "DocumentDate", "sortDirection": 0, "skip": skip}
        res = aps.call("POST", aps.API, body)
        docs = [r["document"] for r in res.get("results", [])]
        yield from docs
        skip += len(docs)
        if not docs or skip >= int(res.get("count") or 0):
            return
        time.sleep(delay)


def kind(title):
    if SKIP.search(title):
        return None
    if REPORT.search(title):
        return "report"
    if ANALYSIS.search(title):
        return "analysis"
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default="asp.db")
    ap.add_argument("--delay", type=float, default=0.5)
    ap.add_argument("--dry-run", action="store_true", help="list what would be fetched")
    a = ap.parse_args()
    con = sqlite3.connect(a.db)
    con.executescript(aps.SCHEMA)
    have = {r[0] for r in con.execute("SELECT accession_number FROM adams_documents WHERE content IS NOT NULL")}

    wanted = {}
    for t in TITLE_SEARCHES:
        n = 0
        for d in title_search(t, a.delay):
            k = kind(d.get("DocumentTitle") or "")
            if k:
                wanted[d["AccessionNumber"]] = (k, d)
                n += 1
        print("title search %r: %d kept" % (t, n), flush=True)
    todo = [acc for acc in wanted if acc not in have]
    by_kind = {}
    for k, _ in wanted.values():
        by_kind[k] = by_kind.get(k, 0) + 1
    print("matched %s; %d already stored, %d to fetch" % (by_kind, len(wanted) - len(todo), len(todo)), flush=True)
    if a.dry_run:
        for acc in todo:
            k, d = wanted[acc]
            print("  %-8s %s %s %s" % (k, acc, d.get("DocumentDate"), (d.get("DocumentTitle") or "")[:90]))
        return
    for i, acc in enumerate(todo, 1):
        _, d = wanted[acc]
        full = aps.get_doc(acc)
        aps.store(con, {**d, **{k: v for k, v in full.items() if k != "content"}}, full.get("content"))
        if i % 25 == 0:
            con.commit()
            print("  fetched %d / %d" % (i, len(todo)), flush=True)
        time.sleep(a.delay)
    con.commit()
    print("done: %d documents in %s" % (con.execute("SELECT count(*) FROM adams_documents").fetchone()[0], a.db))


if __name__ == "__main__":
    main()
