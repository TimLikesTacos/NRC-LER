#!/usr/bin/env python3
"""Parse asp.db (raw ADAMS ASP documents) into asp_parsed.db: one row per precursor.

A precursor is an event the NRC's Accident Sequence Precursor program scored at or above 1e-6:
  CCDP   conditional core damage probability, for an initiating event (trip, loss of offsite
         power, ...): the chance the event would have ended in core damage
  dCDP   increase in core damage probability from a degraded condition over the time it existed
  significant precursor: 1e-3 or more ("1 in 1,000 of leading to a reactor accident")
Some recent precursors are given only as an inspection finding color (White, Yellow, Red)
from the significance determination process instead of a number.

Sources
  report   tables in the yearly results (SECY papers 2001-2015, annual reports 2016-), and the
           table of all significant precursors since 1969. A row is plant, LER (or inspection
           report), event date, value and description, in an order that differs by year, so
           rows are cut at the LER references and the side (before or after the reference)
           whose event date matches the LER's year is taken.
  analysis an individual ASP analysis: the first CCDP/dCDP value stated, for the LER in the
           title (else the first LER cited).
Every mention is kept in asp_mentions. asp_precursors takes, per LER, the value from the most
recent yearly report that lists it (later reports re-state old precursors with revised values,
e.g. Davis-Besse 1985-013: 1e-2 in 2010, 3e-3 in 2016), else the most recent analysis, and
records every value seen.

  python3 parse_asp.py [--raw asp.db] [--out asp_parsed.db] [--show LER]   e.g. --show 346-02-002
"""
import argparse
import collections
import json
import os
import re
import sqlite3

HERE = os.path.dirname(os.path.abspath(__file__))

SCHEMA = """
CREATE TABLE asp_mentions (       -- every (LER, value) read from a document
    accession_number TEXT, doc_date TEXT, source TEXT, docket TEXT, ler_year INTEGER, ler_seq INTEGER,
    value REAL, finding TEXT, event_date TEXT, plant TEXT, context TEXT);
CREATE TABLE asp_precursors (     -- one row per precursor LER
    docket TEXT, ler_year INTEGER, ler_seq INTEGER, ler_key TEXT PRIMARY KEY,
    value REAL,                   -- CCDP or delta-CDP, latest NRC estimate; NULL when only a finding color
    finding TEXT,                 -- White / Yellow / Red / Greater than Green, when given instead
    significant INTEGER,          -- value >= 1e-3
    event_date TEXT, plant TEXT, description TEXT,
    n_sources INTEGER, n_agree INTEGER, values_seen TEXT, sources TEXT);
CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT);
"""

REF = re.compile(r"(?<![\d/-])(?:LERs?\s*(?:No\.?\s*)?)?(?:50-)?(\d{3})\s*[/-]\s*(\d{2}|\d{4})\s*-\s*(\d{3})(?:\s*-\s*\d{2})?(?![\d])")
VALUE = re.compile(r"(?<![\d.])(\d(?:\.\d+)?)\s*[×xX\*]\s*10\s*[-−–]\s*(\d{1,2})(?!\d)")
EVALUE = re.compile(r"(?<![\d.])(\d(?:\.\d+)?)\s*E\s*-\s*0?(\d)(?!\d)")
DATE = re.compile(r"(?<![\d/])(\d{1,2})/(\d{1,2})/(\d{2}|\d{4})(?![\d/])")
FINDING = re.compile(r"\b(white|yellow|red|greater[- ]than[- ]green)\b(?:\s*finding)?", re.I)
THRESHOLD = re.compile(r"(equal to|threshold|greater than|less than|exceed|above|below|at least|≥|>|<|bin|range|between|order of)", re.I)
CDP_WORD = re.compile(r"(CCDP|[ΔD∆]\s?CDP|delta[- ]CDP|conditional core damage probability|increase in core damage probability)", re.I)


def year4(y):
    y = int(y)
    return y if y > 1900 else (1900 + y if y >= 60 else 2000 + y)


def norm(text):
    text = text.replace("−", "-").replace("–", "-").replace("‐", "-")
    return re.sub(r"\s+", " ", text)


def value_of(m):
    return float(m.group(1)) * 10 ** -int(m.group(2))


def first_value(seg):
    m = VALUE.search(seg)
    if m and 1 <= int(m.group(2)) <= 9:
        return value_of(m), m
    return None, None


def date_iso(m):
    mo, d, y = int(m.group(1)), int(m.group(2)), year4(m.group(3))
    if 1 <= mo <= 12 and 1 <= d <= 31:
        return "%04d-%02d-%02d" % (y, mo, d)
    return None


def refs_in(text, dockets):
    out = []
    for m in REF.finditer(text):
        d = "05000" + m.group(1)
        if d in dockets:
            out.append((m, d, year4(m.group(2)), int(m.group(3))))
    return out


def year_ok(iso, ler_year):
    """Event date in the LER's year, or the year before (conditions are often found a year after
    they began)."""
    return bool(iso) and ler_year - 1 <= int(iso[:4]) <= ler_year


def row_fields(before, after, ler_year):
    """(event date, value, finding) of the table row holding one LER reference, from the text
    just before and after it. Layouts by era:
      value right after the LER   date ... plant ... description LER value     (2000s SECY)
      date right after the LER    plant LER date exposure description value    (annual reports)
      date right before the LER   date LER plant description value             (significant list, 2016-)
      otherwise                   plant value date description LER             (significant list, -2015)
    """
    a = after.lstrip(" ;,.)")
    vm = VALUE.match(a)
    if vm:
        dates = [date_iso(m) for m in DATE.finditer(before)]
        dates = [d for d in dates if year_ok(d, ler_year)]
        return (dates[-1], value_of(vm), None) if dates else None
    dm = DATE.match(a)
    if dm and year_ok(date_iso(dm), ler_year):
        rest = a[dm.end():]
        v, _ = first_value(rest[:600])
        f = FINDING.search(rest[:600])
        if f and (v is None or f.start() < VALUE.search(rest).start()):
            return date_iso(dm), None, f.group(1).title()
        return (date_iso(dm), v, None) if v is not None else None
    tail = before.rstrip(" ;,.(")
    dt = [m for m in DATE.finditer(tail) if tail[m.end():].strip() == ""]
    if dt and year_ok(date_iso(dt[-1]), ler_year):
        # date LER plant description value   (significant list, 2016- annual reports)
        v, _ = first_value(after[:900])
        f = FINDING.search(after[:900])
        if v is not None:
            return date_iso(dt[-1]), v, None
        if f:
            return date_iso(dt[-1]), None, f.group(1).title()
        return None
    pairs = []
    for m in VALUE.finditer(before):
        dm = DATE.match(before[m.end():].lstrip())
        if dm and 1 <= int(m.group(2)) <= 9 and year_ok(date_iso(dm), ler_year):
            pairs.append((date_iso(dm), value_of(m), None))
    return pairs[-1] if pairs else None


def report_mentions(acc, text, dockets):
    """Rows of the precursor tables in one yearly report."""
    text = norm(text)
    refs = refs_in(text, dockets)
    out = []
    for i, (m, d, y, s) in enumerate(refs):
        lo = refs[i - 1][0].end() if i else max(0, m.start() - 900)
        hi = refs[i + 1][0].start() if i + 1 < len(refs) else len(text)
        got = row_fields(text[max(lo, m.start() - 900):m.start()], text[m.end():min(hi, m.end() + 900)], y)
        if not got:
            continue
        date, v, finding = got
        ctx = text[max(0, m.start() - 300):m.end() + 300]
        out.append((acc, None, "report", d, y, s, v, finding, date, None, ctx))
    return out


def analysis_mention(acc, title, text, dockets):
    """The precursor value an individual ASP analysis states, for its LER."""
    t = norm(text)
    refs = refs_in(norm(title), dockets) or refs_in(t[:6000], dockets)
    if not refs:
        return []
    _, d, y, s = refs[0]
    if re.search(r"\breject|screen(ed|ing)? out|not a precursor|below the (ASP )?(precursor )?threshold", title + t[:3000], re.I):
        return [(acc, None, "analysis_rejected", d, y, s, None, None, None, None, t[:300])]
    for w in CDP_WORD.finditer(t):
        win = t[w.start():w.end() + 160]
        v, vm = first_value(win)
        if v is None:
            em = EVALUE.search(win)
            v, vm = (value_of(em), em) if em else (None, None)
        # "a CCDP greater than or equal to 1x10-6" states the program threshold, not a result.
        if vm and THRESHOLD.search(win[:vm.start()]):
            continue
        if v is not None and 1e-9 <= v <= 1:
            return [(acc, None, "analysis", d, y, s, v, None, None, None, t[max(0, w.start() - 200):w.end() + 200])]
    return []


def is_report(title):
    return bool(re.search(r"annual report|status (report )?(of|on) (the )?accident sequence|results,? trends|program results", title, re.I))


def round_sig(v):
    return float("%.1g" % v) if v else v


def combine(mentions):
    """asp_precursors rows: per LER, the latest report's value, else the latest analysis's."""
    by = collections.defaultdict(list)
    for m in mentions:
        if m[2] != "analysis_rejected":
            by[(m[3], m[4], m[5])].append(m)
    rows = []
    for (d, y, s), ms in by.items():
        newest = lambda src: sorted((m for m in ms if m[2] == src and (m[6] is not None or m[7])),
                                    key=lambda m: (m[1] or "", m[0]), reverse=True)
        pick = (newest("report") or newest("analysis") or [None])[0]
        value = round_sig(pick[6]) if pick and pick[6] is not None else None
        # Every significant precursor (>= 1e-3) is in the yearly reports' list; one only an
        # analysis gives is an OCR misread of the exponent (1993 scans: "7.1 x 10-1").
        if value is not None and value >= 1e-3 and pick[2] != "report":
            value = None
        finding = pick[7] if pick and value is None else None
        vals = collections.Counter(round_sig(m[6]) for m in ms if m[6] is not None)
        dates = collections.Counter(m[8] for m in ms if m[8])
        rows.append((d, y, s, "%s:%04d-%03d" % (d, y, s), value, finding,
                     int(value is not None and value >= 1e-3),
                     dates.most_common(1)[0][0] if dates else None, NO_LER_PLANTS.get(d), None,
                     len({m[0] for m in ms}), sum(1 for m in ms if m[6] is not None and round_sig(m[6]) == value),
                     json.dumps({"%g" % k: n for k, n in vals.items()}), json.dumps(sorted({m[0] for m in ms}))))
    return rows


# Power reactors shut down before their LERs reached the ADAMS main library, so they have no row in
# units; their precursors (Rancho Seco 1978-001 is one of the riskiest ever) would otherwise be dropped.
NO_LER_PLANTS = {"05000312": "Rancho Seco", "05000409": "La Crosse"}


def dockets_known():
    db = sqlite3.connect("file:%s?mode=ro" % os.path.join(HERE, "lers_parsed.db"), uri=True)
    return {r[0] for r in db.execute("SELECT docket FROM units")} | set(NO_LER_PLANTS)


def parse_all(raw):
    dockets = dockets_known()
    src = sqlite3.connect("file:%s?mode=ro" % raw, uri=True)
    mentions = []
    for acc, title, content, doc_date in src.execute(
            "SELECT accession_number, coalesce(document_title, ''), content, document_date FROM adams_documents "
            "WHERE content IS NOT NULL"):
        found = report_mentions(acc, content, dockets) if is_report(title) else analysis_mention(acc, title, content, dockets)
        mentions += [(m[0], (doc_date or "")[:10] or None) + m[2:] for m in found]
    return mentions


def build(raw, out):
    mentions = parse_all(raw)
    rows = combine(mentions)
    tmp = out + ".tmp"
    if os.path.exists(tmp):
        os.remove(tmp)
    db = sqlite3.connect(tmp)
    db.executescript(SCHEMA)
    db.executemany("INSERT INTO asp_mentions VALUES (?,?,?,?,?,?,?,?,?,?,?)", mentions)
    db.executemany("INSERT INTO asp_precursors VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)", rows)
    db.execute("INSERT INTO meta VALUES ('built_from', ?)", (raw,))
    db.commit()
    db.close()
    os.replace(tmp, out)
    kinds = collections.Counter(m[2] for m in mentions)
    sig = sum(r[6] for r in rows)
    print("built %s: %d mentions %s -> %d precursors (%d significant, %d finding color only)"
          % (out, len(mentions), dict(kinds), len(rows), sig, sum(1 for r in rows if r[4] is None)))


def show(raw, ler):
    d, y, s = re.match(r"(\d{3})-(\d{2,4})-(\d{3})", ler).groups()
    key = ("05000" + d, year4(y), int(s))
    for m in parse_all(raw):
        if (m[3], m[4], m[5]) == key:
            print("%s %s %-17s value=%s finding=%s date=%s\n    %s" % (m[0], m[1], m[2], m[6], m[7], m[8], m[10][:400]))


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--raw", default=os.path.join(HERE, "asp.db"))
    ap.add_argument("--out", default=os.path.join(HERE, "asp_parsed.db"))
    ap.add_argument("--show", metavar="LER")
    a = ap.parse_args()
    if a.show:
        show(a.raw, a.show)
    else:
        build(a.raw, a.out)
