#!/usr/bin/env python3
"""Data-quality checks on lers_analysis.db, compared with the previous run.

  python3 check_data.py              # print metrics, compare with check_data_last.json, save
  python3 check_data.py --no-save    # compare only
  python3 check_data.py --show NAME  # list the rows behind one metric
  python3 check_data.py --accept     # save as baseline after a reviewed, deliberate change

Each metric has a direction (lower or higher is better) and a tolerance. The script exits 1 when
a metric is worse than the last saved run by more than its tolerance, or breaks a hard limit,
so refresh.py can stop before a bad build is used.

Read-only on the databases.
"""
import argparse
import json
import os
import re
import sqlite3
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DB = os.path.join(HERE, "lers_analysis.db")
LAST = os.path.join(HERE, "check_data_last.json")

# "LER 50-004-00" is a docket prefix with the year left out, not 1950 (LERs start in 1969)
TITLE_LER = re.compile(r"\bLER\s*(?:No\.?\s*)?(?!50-)(\d{2}|\d{4})\s*-\s*(\d{2,3})\s*-\s*(\d{2})\b", re.I)


def year4(y):
    y = int(y)
    return y if y > 1900 else (y + 2000 if y < 50 else y + 1900)


def title_mismatches(db):
    """Documents whose ADAMS title names a different LER number than the parser chose."""
    latest = {r[0] for r in db.execute("SELECT accession_number FROM ler_latest")}
    out = []
    for acc, num, title in db.execute(
            "SELECT l.accession_number, l.ler_number, COALESCE(t.title, l.title) FROM lers l "
            "LEFT JOIN adams_titles t USING (accession_number)"):
        m = TITLE_LER.search(title or "")
        if not m:
            continue
        y, s, r = m.groups()
        t = "%04d-%03d-%02d" % (year4(y), int(s), int(r))
        if num != t:
            out.append((acc, num, t, acc in latest, (title or "")[:70]))
    return out


def sequence_gaps(db, since=2000):
    """Missing sequence numbers below a docket's highest one in a year (since `since`)."""
    rows = db.execute("SELECT docket, ler_year, group_concat(DISTINCT ler_seq) FROM lers "
                      "WHERE docket IS NOT NULL AND ler_year >= ? GROUP BY 1, 2", (since,)).fetchall()
    gaps = []
    for docket, year, seqs in rows:
        have = {int(x) for x in seqs.split(",")}
        gaps += [(docket, year, n) for n in range(1, max(have)) if n not in have]
    return gaps


def q(db, sql, *args):
    return db.execute(sql, args).fetchall()


def one(db, sql, *args):
    return db.execute(sql, args).fetchone()[0]


# form text that trim_abstract() in parse_lers.py removes (LIKE is case-insensitive for ASCII)
ABSTRACT_FORM_MARKERS = ["Estimated burden per response", "APPROVED BY OMB", "Check all that apply",
                         "SUPPLEMENTAL REPORT EXPECTED", "EXPECTED SUBMISSION", "REPORTABLE TO EPIX",
                         "20.2201(b) 20.2203"]   # not "LICENSEE EVENT REPORT (LER)": licensees write it in prose

# name: (description, direction "lower"/"higher" is better, tolerance vs last run, hard limit or None)
METRICS = {
    "title_mismatch_hidden": ("title disagrees with parsed LER number AND the doc is hidden from events",
                              "lower", 0, 25),
    "title_mismatch": ("title disagrees with parsed LER number", "lower", 5, None),
    "docketless_keys": ("LER keys with no docket or accession ('?:'), which collide across licensees", "lower", 0, 0),
    "non_reactor_events": ("events with no reporting unit (materials-license reports leaking into events)", "lower", 0, 0),
    "sequence_gaps_2000": ("missing sequence numbers per docket-year since 2000", "lower", 3, None),
    "unclassified_power": ("latest PWR/BWR events with text but no classification", "lower", 0, 0),
    "event_date_pct": ("latest events with an event date (%)", "higher", 0.5, None),
    "ler_number_pct": ("documents with an LER number (%)", "higher", 0.5, None),
    "event_after_report": ("events dated after their report date", "lower", 2, None),
    "dup_latest_revision": ("LER keys whose latest revision is filed twice (same rev, two documents)", "lower", 3, None),
    "units_without_site": ("units with no site (plant matching can't find them)", "lower", 0, 0),
    "unknown_dockets": ("dockets in LERs missing from plants_ref.py UNITS (add them by hand)", "lower", 0, None),
    "abstract_form_text": ("latest events whose abstract still holds Form 366 labels or page-2 boilerplate", "lower", 3, None),
    "abstract_pct": ("latest events with an abstract (%)", "higher", 0.5, None),
    "cfr_list_noise": ("documents citing more than 8 reporting criteria (printed form list leaking in)", "lower", 2, None),
    "asp_precursors": ("NRC precursors parsed from the ASP results", "higher", 3, None),
    "asp_matched_2000_pct": ("precursors since 2000 whose LER is in the database (%)", "higher", 2, None),
    "alias_collisions": ("aliases used by more than one site", "lower", 0, 0),
    "events": ("events in the events view", "higher", 0, None),
}


def compute(db):
    mism = title_mismatches(db)
    return {
        "title_mismatch_hidden": sum(1 for m in mism if not m[3]),
        "title_mismatch": len(mism),
        "docketless_keys": one(db, "SELECT count(*) FROM lers WHERE ler_key LIKE '?:%'"),
        "non_reactor_events": one(db, "SELECT count(*) FROM events WHERE docket IS NULL"),
        "sequence_gaps_2000": len(sequence_gaps(db)),
        "unclassified_power": one(db, "SELECT count(*) FROM ler_latest l LEFT JOIN classification c USING (accession_number) "
                                      "WHERE l.reactor_type IN ('PWR','BWR') AND l.content_chars >= 500 AND c.accession_number IS NULL"),
        "event_date_pct": round(100.0 * one(db, "SELECT avg(event_date IS NOT NULL) FROM ler_latest WHERE docket IS NOT NULL"), 1),
        "ler_number_pct": round(100.0 * one(db, "SELECT avg(ler_number IS NOT NULL) FROM lers"), 1),
        "event_after_report": one(db, "SELECT count(*) FROM ler_latest WHERE docket IS NOT NULL AND event_date > report_date"),
        "dup_latest_revision": one(db, "SELECT count(*) FROM (SELECT l.ler_key FROM lers l JOIN ler_latest x "
                                       "ON x.ler_key = l.ler_key AND x.ler_rev = l.ler_rev WHERE l.docket IS NOT NULL "
                                       "GROUP BY l.ler_key HAVING count(*) > 1)"),
        "units_without_site": one(db, "SELECT count(*) FROM units WHERE site_id IS NULL OR site_id NOT IN (SELECT site_id FROM sites)"),
        "unknown_dockets": one(db, "SELECT count(DISTINCT docket) FROM lers WHERE docket IS NOT NULL "
                                   "AND docket NOT IN (SELECT docket FROM units)"),
        "abstract_form_text": one(db, "SELECT count(*) FROM ler_latest WHERE docket IS NOT NULL AND (" + " OR ".join(
            "abstract LIKE '%%%s%%'" % m for m in ABSTRACT_FORM_MARKERS) + ")"),
        "abstract_pct": round(100.0 * one(db, "SELECT avg(abstract IS NOT NULL) FROM ler_latest WHERE docket IS NOT NULL"), 1),
        "cfr_list_noise": one(db, "SELECT count(*) FROM (SELECT accession_number FROM ler_cfr GROUP BY 1 HAVING count(*) > 8)"),
        "asp_precursors": one(db, "SELECT count(*) FROM asp_precursors"),
        "asp_matched_2000_pct": round(100.0 * (one(db, "SELECT avg(ler_key IN (SELECT ler_key FROM ler_latest)) "
                                                       "FROM asp_precursors WHERE ler_year >= 2000") or 0), 1),
        "alias_collisions": one(db, "SELECT count(*) FROM (SELECT lower(alias) FROM site_aliases GROUP BY 1 "
                                    "HAVING count(DISTINCT site_id) > 1)"),
        "events": one(db, "SELECT count(*) FROM events"),
    }


def info(db):
    return {
        "newest_report_date": one(db, "SELECT max(report_date) FROM ler_latest WHERE report_date <= date('now')"),
        "newest_document_added": one(db, "SELECT max(date_added) FROM lers"),
        "built_at": one(db, "SELECT v FROM meta WHERE k = 'built_at'"),
        "parser_version": one(db, "SELECT v FROM meta WHERE k = 'parser_version'"),
    }


def show(db, name):
    if name.startswith("title_mismatch"):
        for m in title_mismatches(db):
            if name == "title_mismatch" or not m[3]:
                print("%s parsed=%s title=%s latest=%s  %s" % m)
    elif name == "sequence_gaps_2000":
        for d, y, n in sequence_gaps(db):
            print("%s %d-%03d" % (d, y, n))
    elif name == "abstract_form_text":
        for r in q(db, "SELECT accession_number, substr(replace(abstract, char(10), ' '), 1, 100) FROM ler_latest "
                       "WHERE docket IS NOT NULL AND (" + " OR ".join("abstract LIKE '%%%s%%'" % m
                                                              for m in ABSTRACT_FORM_MARKERS) + ")"):
            print(*r)
    elif name == "unknown_dockets":
        for r in q(db, "SELECT docket, count(*), min(ler_year), max(ler_year) FROM lers WHERE docket IS NOT NULL "
                       "AND docket NOT IN (SELECT docket FROM units) GROUP BY 1"):
            print(*r)
    elif name == "asp_matched_2000_pct":
        for r in q(db, "SELECT ler_key, value, finding, event_date FROM asp_precursors WHERE ler_year >= 2000 "
                       "AND ler_key NOT IN (SELECT ler_key FROM ler_latest)"):
            print(*r)
    elif name == "event_after_report":
        for r in q(db, "SELECT accession_number, event_date, report_date FROM ler_latest WHERE event_date > report_date"):
            print(*r)
    elif name == "unclassified_power":
        for r in q(db, "SELECT l.accession_number, l.ler_number FROM ler_latest l LEFT JOIN classification c USING (accession_number) "
                       "WHERE l.reactor_type IN ('PWR','BWR') AND l.content_chars >= 500 AND c.accession_number IS NULL"):
            print(*r)
    else:
        sys.exit("no row listing for %s" % name)


def judge(name, now, last):
    _, direction, tol, limit = METRICS[name]
    worse = 0 if last is None else (now - last) if direction == "lower" else (last - now)
    if limit is not None and ((direction == "lower" and now > limit) or (direction == "higher" and now < limit)):
        return "FAIL", "beyond limit %s" % limit
    if last is not None and worse > tol:
        return "FAIL", "worse by %s (tolerance %s)" % (round(worse, 1), tol)
    if last is not None and worse < 0:
        return "ok", "better"
    return "ok", ""


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default=DB)
    ap.add_argument("--no-save", action="store_true")
    ap.add_argument("--accept", action="store_true", help="save as the new baseline even if checks fail "
                    "(after a deliberate change you have reviewed)")
    ap.add_argument("--show", metavar="METRIC")
    a = ap.parse_args()
    db = sqlite3.connect("file:%s?mode=ro" % a.db, uri=True)
    if a.show:
        return show(db, a.show)
    now, meta = compute(db), info(db)
    last = {}
    if os.path.exists(LAST):
        with open(LAST) as f:
            last = json.load(f).get("metrics", {})
    print("data: newest report %(newest_report_date)s, documents added through %(newest_document_added)s, "
          "built %(built_at)s, parser v%(parser_version)s" % meta)
    failed = False
    for name, (desc, *_rest) in METRICS.items():
        prev = last.get(name)
        status, why = judge(name, now[name], prev)
        failed |= status == "FAIL"
        print("%-4s %-22s %8s  (last %s)  %s%s" % (status, name, now[name], "-" if prev is None else prev, desc,
                                                  "  <- " + why if why else ""))
    if (not failed or a.accept) and not a.no_save:
        with open(LAST, "w") as f:
            json.dump({"metrics": now, "info": meta}, f, indent=1)
    if failed and a.accept:
        print("\naccepted as the new baseline despite failures (--accept)")
        return 0
    if failed:
        print("\nFAILED: not saving; investigate with --show METRIC, or --accept a reviewed change")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main() or 0)
