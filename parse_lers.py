#!/usr/bin/env python3
"""Parse raw LERs (lers.db, never modified) into a rebuildable lers_parsed.db.

Usage:
  python3 parse_lers.py [--raw lers.db] [--out lers_parsed.db]
  python3 parse_lers.py --show ACCESSION      # print the parse of one document
  python3 parse_lers.py --coverage            # field fill rates by decade (after a build)

The output file is deleted and rebuilt on every run.
"""
import argparse
import collections
import datetime as dt
import json
import os
import re
import sqlite3
import sys

from plants_ref import ALIASES, DOCKET_RE, SITES, UNITS, unit_rows

PARSER_VERSION = "11"

SCHEMA = """
CREATE TABLE lers (
    accession_number  TEXT PRIMARY KEY,
    ler_number        TEXT,     -- normalized YYYY-SSS-RR of the primary LER in the document
    ler_year          INTEGER,
    ler_seq           INTEGER,
    ler_rev           INTEGER,
    ler_key           TEXT,     -- docket:YYYY-SSS  (groups revisions of one event; docket is a non-reactor
                                --   ADAMS docket, or ?ACCESSION, when there is no reporting unit)
    docket            TEXT,     -- reporting unit, 8-digit (form box 2, else first ADAMS docket)
    site_id           TEXT,     -- sites.site_id of the reporting unit
    facility          TEXT,     -- facility name as written on the form (or from the title)
    licensee          TEXT,     -- ADAMS author affiliation
    title             TEXT,
    event_date        TEXT,     -- ISO
    report_date       TEXT,     -- ISO (form block 7, else ADAMS document date)
    document_date     TEXT,     -- ADAMS document date
    date_added        TEXT,
    operating_mode    TEXT,
    power_level       INTEGER,  -- percent
    form_type         TEXT,     -- '366-numbered', '366-legacy', 'pre-366/other'
    abstract          TEXT,
    narrative         TEXT,     -- continuation-sheet text with form boilerplate removed
    sec_plant_id      TEXT,
    sec_condition_prior TEXT,
    sec_description   TEXT,
    sec_cause         TEXT,
    sec_safety        TEXT,
    sec_corrective    TEXT,
    sec_previous      TEXT,
    sec_reportability TEXT,
    sec_component     TEXT,
    reactor_type      TEXT,     -- of the reporting unit: PWR / BWR / FBR / research / ISFSI (plants_ref.py)
    reactor_vendor    TEXT,
    content_chars     INTEGER,
    low_text          INTEGER,  -- 1 if raw content < 2000 chars
    pdf_url           TEXT,
    parse_quality     TEXT      -- JSON: which fields were found and from where
);
CREATE TABLE ler_numbers (     -- every LER number a document refers to (some bundle several)
    accession_number TEXT, ler_number TEXT, ler_key TEXT, is_primary INTEGER,
    PRIMARY KEY (accession_number, ler_number)
);
CREATE TABLE ler_components (  -- form block 13 rows
    accession_number TEXT, line INTEGER, cause TEXT, system TEXT, component TEXT,
    manufacturer TEXT, reportable TEXT
);
CREATE TABLE ler_cfr (         -- reporting criteria cited in abstract/narrative
    accession_number TEXT, cfr_section TEXT, PRIMARY KEY (accession_number, cfr_section)
);
CREATE TABLE ler_units (       -- every unit a document names; affected = our best call on involvement
    accession_number TEXT, docket TEXT, site_id TEXT,
    role TEXT,                 -- reporting / other
    in_adams INTEGER,          -- ADAMS filed the document under this docket
    in_form_box2 INTEGER,      -- form box 2 (reporting facility docket)
    in_form_box8 INTEGER,      -- form box 8 (other facilities involved)
    in_title INTEGER,          -- title says e.g. "Units 1 and 2"
    affected INTEGER,
    affected_basis TEXT,       -- form: boxes 2+8 (+title) were readable; adams: fell back to ADAMS dockets
    PRIMARY KEY (accession_number, docket)
);
CREATE TABLE sites (site_id TEXT PRIMARY KEY, name TEXT, state TEXT);
CREATE TABLE site_aliases (site_id TEXT, alias TEXT);   -- abbreviations people type (SQN, PVNGS)
CREATE TABLE units (
    docket TEXT PRIMARY KEY, site_id TEXT, unit TEXT, name TEXT, reactor_type TEXT, reactor_vendor TEXT,
    operating INTEGER,         -- on the NRC operating list (nrc_operating_units.tsv)
    owner_operator TEXT, nrc_region TEXT, license TEXT
);
CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT);
"""

POST_SCHEMA = """
CREATE INDEX idx_lers_key ON lers(ler_key);
CREATE INDEX idx_lers_docket ON lers(docket);
CREATE INDEX idx_lers_event ON lers(event_date);
CREATE INDEX idx_comp_sys ON ler_components(system);
CREATE INDEX idx_cfr ON ler_cfr(cfr_section);
-- latest revision of each event
CREATE VIEW ler_latest AS
  SELECT l.* FROM lers l
  WHERE l.ler_key IS NOT NULL AND l.accession_number = (
     SELECT l2.accession_number FROM lers l2 WHERE l2.ler_key = l.ler_key
     ORDER BY l2.ler_rev DESC, l2.document_date DESC, l2.accession_number DESC LIMIT 1);
CREATE INDEX idx_lers_site ON lers(site_id);
CREATE INDEX idx_lu_docket ON ler_units(docket, affected);
CREATE INDEX idx_lu_site ON ler_units(site_id);
-- per-unit and per-site summaries (documents, not deduplicated across revisions)
CREATE VIEW unit_stats AS
  SELECT u.docket, u.site_id, u.name, u.reactor_type, u.operating,
         (SELECT COUNT(*) FROM lers l WHERE l.docket = u.docket) AS n_reported,
         (SELECT COUNT(*) FROM ler_units x WHERE x.docket = u.docket AND x.affected) AS n_affected
  FROM units u;
CREATE VIEW site_stats AS
  SELECT s.site_id, s.name, s.state,
         (SELECT COUNT(*) FROM units u WHERE u.site_id = s.site_id) AS n_units,
         (SELECT GROUP_CONCAT(DISTINCT reactor_type) FROM units u WHERE u.site_id = s.site_id) AS reactor_types,
         COUNT(l.accession_number) AS n_docs, MIN(l.event_date) AS first_event, MAX(l.event_date) AS last_event
  FROM sites s LEFT JOIN lers l ON l.site_id = s.site_id GROUP BY s.site_id;
CREATE VIRTUAL TABLE lers_fts USING fts5(
  accession_number UNINDEXED, title, abstract, narrative, tokenize='porter');
INSERT INTO lers_fts SELECT accession_number, title, abstract, COALESCE(narrative, '') FROM lers;
"""

# ------------------------------------------------------------------ helpers
WS = re.compile(r"[ \t]+")


def clean(s):
    if s is None:
        return None
    s = WS.sub(" ", s)
    s = re.sub(r" *\n *", "\n", s)
    s = re.sub(r"\n{3,}", "\n\n", s).strip()
    return s or None


def oneline(s):
    s = clean(s)
    return re.sub(r"\s+", " ", s).strip() if s else None


def year4(y):
    y = int(y)
    if y < 100:
        y += 1900 if y >= 60 else 2000
    return y


def iso(m, d, y):
    try:
        return dt.date(year4(y), int(m), int(d)).isoformat()
    except (ValueError, TypeError):
        return None


# "LER 2014-002-00", "LER 14-002", "LER-02-01-00", "LER 2023-04-00" (two-digit sequences: FPL/NextEra,
# River Bend, older Ginna/Robinson). The lookaheads reject runs like "09-0076" or "15-00200" instead of
# reading a wrong number out of them.
# A revision is two digits not followed by a digit or letter: in 1970s numbers like "78-001/03L-0"
# or "1978-001-03L-0", "03L" is a report-type code, not revision 03.
LER_NO = re.compile(r"\bLER\s*-?\s*(?:No\.?\s*)?(?:\d-)?(\d{2}|\d{4})\s*-\s*(\d{2,3})(?!\d)"
                    r"(?:\s*-\s*(\d{2})(?![\dA-Z]))?", re.I)
# 1970s docket form: "LER 50-263/75-15", "RO 50-263/76-06" -> 1975-015, 1976-006
LER_NO_DOCKET = re.compile(r"\b(?:LER|RO)\s*(?:No\.?\s*)?0?5?0\s*-\s*\d{3}\s*/\s*(\d{2})\s*-\s*(\d{2,3})(?!\d)",
                           re.I)
FIRST_LER_YEAR = 1969
LAST_LER_YEAR = dt.date.today().year + 1


def norm_ler(y, s, r):
    return "%04d-%03d-%02d" % (year4(y), int(s), int(r or 0))


def valid_ler(n):
    """Rejects impossible numbers such as "1875-027" (an ADAMS typo) or sequence 000."""
    y, s, _ = n.split("-")
    return FIRST_LER_YEAR <= int(y) <= LAST_LER_YEAR and int(s) > 0


def ler_numbers_in(text):
    """Distinct valid LER numbers in text, in order."""
    found = [(m.start(), norm_ler(*m.groups())) for m in LER_NO.finditer(text or "")]
    found += [(m.start(), norm_ler(m.group(1), m.group(2), None)) for m in LER_NO_DOCKET.finditer(text or "")]
    nums = []
    for _, n in sorted(found):
        if valid_ler(n) and n not in nums:
            nums.append(n)
    return nums


# Loose citations in letter text: "R.O. 77-13", "335-77-13", "LER 2001-002" -> (77, 13), (1, 2).
CITED_NO = re.compile(r"(?<![\d/])(\d{2}|\d{4})\s*-\s*0*(\d{1,3})(?!\d)")


def cited_numbers(text):
    """(two-digit year, sequence) pairs cited anywhere in text."""
    return {(int(y) % 100, int(s)) for y, s in CITED_NO.findall(text or "")}


def form_misread(form_ler, meta, event_date, cited=frozenset()):
    """The form's LER number is an OCR misread, given that the report-number field and the title
    agree on `meta`: a revision of 6 or more ("2000-100-21"), a sequence of 50+ where meta's is
    lower ("2018-050-00" for 001), a year that misses the event's when meta's matches ("2011-004"
    for a 2017 event), or a different sequence when only meta's is cited in the text. A form that
    only disagrees on a small revision stays: ADAMS titles often lag supplements."""
    fy, fs, fr = (int(x) for x in form_ler.split("-"))
    my, ms, _ = (int(x) for x in meta.split("-"))
    ev_year = int((event_date or "0")[:4] or 0)
    if fr >= 6 or (fs >= 50 and ms < 50):
        return True
    if fy != my and my == ev_year:
        return True
    return fs != ms and (my % 100, ms) in cited and (fy % 100, fs) not in cited


def choose_ler(form_ler, rep_nums, title_nums, event_date, cited=frozenset(), document_date=None):
    """Pick the document's primary LER number. Returns (number, source, rejected numbers).

    Checked against ADAMS titles, the hand-keyed ADAMS report-number field is the least reliable
    source (e.g. Columbia 2001-002 indexed as "00-002-00"), so the order is:
      1. the number on the form itself, when the event-date/LER-number row could be read, unless the
         report-number field and the title agree on another number and the form's is a misread
         (form_misread);
      2. the report-number field and the title, when they agree;
      3. whichever of title / report-number field has the event's year;
      4. whichever one the document's own text cites (year and sequence, see CITED_NO);
      5. by era: before 1990 the report-number field (the titles of 1970s documents, keyed when
         ADAMS indexed them later, have typos such as "76-963" for 76-063), else the title.
    """
    rep, title = (rep_nums or [None])[0], (title_nums or [None])[0]
    if form_ler and valid_ler(form_ler):
        if rep and rep == title and rep != form_ler and form_misread(form_ler, rep, event_date, cited):
            return rep, "meta_over_form", [form_ler]
        rejected = [n for n in rep_nums[:1] + title_nums[:1] if n != form_ler]
        return form_ler, "form", sorted(set(rejected))
    if rep and rep == title:
        return rep, "meta", []
    cands = [(n, src) for n, src in ((title, "title"), (rep, "report_number")) if n]
    if not cands:
        return None, None, []
    ev_year = (event_date or "")[:4]
    in_text = [(n, s) for n, s in cands if (int(n[:4]) % 100, int(n[5:8])) in cited]
    by_era = cands[-1] if (document_date or "9999") < "1990" else cands[0]
    pick = (next(((n, s) for n, s in cands if n[:4] == ev_year), None)
            or (in_text[0] if len(in_text) == 1 else None)
            or by_era)
    return pick[0], pick[1], [n for n, _ in cands if n != pick[0]]


# ------------------------------------------------------------------ form block 1-10
FORM366 = re.compile(r"N\w?C\s+FORM\s+366(?!\s*A)", re.I)
# event date | LER number | report date row, e.g. "11 07 2023 2023 - 002 - 00 01 08 2024"
DATE_ROW = re.compile(
    r"(?<!\d)(\d{1,2})\s+(\d{1,2})\s+(\d{2}|\d{4})\s+(\d{2}|\d{4})\s*[-–]?\s*(\d{3})\s*[-–]?\s*(\d{2})"
    r"\s+(\d{1,2})\s+(\d{1,2})\s+(\d{4}|\d{2})")
LBL_FACILITY = re.compile(r"(?:1\s*\.\W{0,3}FACILITY\s+NAME|FACILITY\s+NAME\s*\(1\))", re.I)
LBL_DOCKET = re.compile(r"(?:2\.\s*DOCKET\s+NUMBER|DOCKET\s+NUMBER\s*\(2\))", re.I)
LBL_TITLE = re.compile(r"(?:4\s*\.\W{0,3}TITLE|TITLE\s*\(4\))", re.I)
LBL_EVENTDATE = re.compile(r"(?:5\.\s*EVENT\s+DATE|EVENT\s+DATE\s*\(5\))", re.I)
LBL_OTHERFAC = re.compile(r"OTHER\s+FACILIT\w*\s+INVOLVED", re.I)
LBL_MODE = re.compile(r"(?:9\.\s*OPERATING\s+MODE|OPERATING\s*_?\s*(?:\S+\s+)?MODE\s*\(9\))", re.I)
LBL_POWER = re.compile(r"(?:10\.\s*POWER\s+LEVEL|LEVEL\s*\(10\))", re.I)
LBL_ABSTRACT = re.compile(
    r"(?:16\.\s*ABSTRACT|ABSTRACT\s*\(Limit[^)]*\)\s*(?:\(16\))?|ABSTRACT\s*\(16\))[^\n]*", re.I)
LBL_BLOCK13 = re.compile(r"COMPLETE\s+ONE\s+L\w*NE\s+FOR\s+EACH\s+COMPONENT", re.I)
LBL_BLOCK14 = re.compile(r"SUPPLEMENTAL\s+REPO", re.I)
ABSTRACT_END = re.compile(
    r"\(See Page 2|\(See NUREG-1022|NRC\s+FORM\s+366|\(If more space is required|CONTINUATION\s+SHEET",
    re.I)
COMP_ROW = re.compile(r"(?<![A-Z0-9])([A-EX])\s+([A-Z]{2})\s+([A-Z]{2,4})\s+([A-Z]\d{3})\s+([YN])(?![A-Z0-9])")
NARRATIVE_START = re.compile(r"CONTINUATION\s+SHEET|TEXT\s+CONTINUATION|^[^\S\n]*N\w?C\s+FORM\s+366A", re.I | re.M)

BOILERPLATE = re.compile("|".join([
    r"NRC\s+FORM\s+366A?\b.*", r"U\.?\s*S\.?\s+NUCLEAR\s+REGULATORY\s+COMMISSION.*",
    r"APPROVED\s+BY\s+OMB.*", r"EXPIRES[:\s].*", r"Estimated\s+burden.*",
    r".*(?:lessons\s+learned|burden\s+estimate|Infocollects|oira_submission|Desk\s+Officer|"
    r"Information\s+Collections\s+Branch|Records\s+Management\s+Branch|Office\s+of\s+Management\s+and\s+Budget|"
    r"Regulatory\s+Affairs|not\s+conduct\s+or\s+sponsor|currently\s+valid\s+OMB|"
    r"collection\s+of\s+information|information\s+collection|NUREG-1022|sr1022|"
    r"required\s+number\s+of\s+digits|725\s+17th\s+Street|Washington,\s*DC\s+20\d{3}).*",
    r"LICENSEE\s+EVENT\s+REPORT\s+\(LER\)\s*", r"CONTINUATION\s+SHEET\s*", r"TEXT\s+CONTINUATION\s*",
    r"\(\d{1,2}-\d{2,4}\)\s*", r"Page\s+\d+\s+of\s+\d+\s*",
]), re.I)


# continuation-sheet page header: "FACILITY NAME ... DOCKET ... LER NUMBER ... <facility> 050003xx ... 2012 - 003 - 00"
PAGE_HEADER = re.compile(
    r"^[^\n]*\bFACILITY\s+NAME\b[^\n]*\n(?:[^\n]*\n){0,15}?"
    r"(?:[^\n]*\bNARRATIVE\b[^\n]*|[^\S\n]*TEXT\b[^\n]*"
    r"|[^\n]*?(?:\d{2}|\d{4})\s*-\s*[\d \t]{3,7}\s*-\s*\d[ \t]?\d\b[^\n]*(?:\n[^\S\n]*(?:NARRATIVE|TEXT)\b[^\n]*)?)\n",
    re.I | re.M)


def strip_boilerplate(text):
    text = PAGE_HEADER.sub("\n", text + "\n")
    out = []
    for line in text.splitlines():
        if BOILERPLATE.fullmatch(line.strip()):
            continue
        out.append(line)
    return "\n".join(out)


def first_line_after(label_re, text, stop_res, maxlen=300):
    m = label_re.search(text)
    if not m:
        return None
    seg = text[m.end(): m.end() + maxlen]
    for sr in stop_res:
        s = sr.search(seg)
        if s:
            seg = seg[: s.start()]
    return seg


PLACEHOLDER_DOCKET = "99990001"   # ADAMS "no docket" placeholder


def event_key_docket(docket, other_dockets, acc):
    """Docket part of ler_key: the reporting unit, else the first non-reactor ADAMS docket (materials
    licensees number their LERs per license, so two licensees can both file 2005-022), else
    '?' + accession, which never groups with another document. Only reactor and ISFSI events
    (rec docket set) reach the events view."""
    if docket:
        return docket
    if other_dockets:
        return other_dockets[0]
    return "?" + acc


def norm_docket(m):
    """8-digit docket from a DOCKET_RE match, only if it is a known unit."""
    pre = "05200" if re.match(r"0?52", m.group(0)) else "05000"
    d = pre + m.group(1)
    return d if d in UNITS else None


def find_dockets(text):
    """Known dockets in text, in order; also tries with spaces between digits removed (0 5 0 0 0 2 5 4)."""
    out = []
    for t in (text, re.sub(r"(?<=\d)[ \t]+(?=\d)", "", text)):
        for m in re.finditer(DOCKET_RE, t):
            d = norm_docket(m)
            if d and d not in out:
                out.append(d)
        if out:
            break
    return out


UNITS_IN_TITLE = re.compile(r"\bUnits?\s+(\d)(?:\s*(?:,|&|and)\s*(\d))?(?:\s*,?\s*(?:&|and)\s*(\d))?", re.I)


def title_dockets(title, site):
    """Dockets of a site named in a title like 'Units 1 and 2'; only when the title names 2+ units."""
    m = UNITS_IN_TITLE.search(title or "")
    if not m or not site:
        return []
    labels = {g for g in m.groups() if g}
    if len(labels) < 2:
        return []
    return sorted(d for d, u in UNITS.items() if u[0] == site and u[1] in labels)


def parse_form(text, candidates=()):
    """Return dict of fields from page 1 of Form 366, plus the narrative text."""
    r, q = {}, {}
    starts = [m.start() for m in (FORM366.search(text), LBL_FACILITY.search(text)) if m]
    if not starts:
        return r, q, None
    form = text[min(starts):]
    am0 = LBL_ABSTRACT.search(form, 0, 15000)
    page1_end = NARRATIVE_START.search(form, am0.end() if am0 else 200)
    page1 = form[: page1_end.start()] if page1_end else form[:12000]
    r["form_type"] = "366-numbered" if re.search(r"1\.\s*FACILITY\s+NAME", page1, re.I) else "366-legacy"

    # facility: text after label up to docket label / 8-digit docket
    fm = LBL_FACILITY.search(page1)
    seg = None
    if fm:
        seg = page1[fm.end(): fm.end() + 300]
        lines = seg.split("\n")
        if re.search(r"DOCKET|PAGE|\(2\)|\(3\)", lines[0], re.I):
            lines = lines[1:]
        seg = next((l for l in lines if l.strip()), "")
        seg = re.split(r"\b0?5000\d{3}\b|\b050\b|\n\s*\d\.\s|\bPAGE\b|\bDOCKET\b|\n", seg, maxsplit=1, flags=re.I)[0]
    if seg:
        fac = oneline(seg)
        if fac and 3 < len(fac) < 120 and re.search(r"[a-z]", fac, re.I):
            r["facility"] = fac.strip(" |:-")
            q["facility"] = "form"

    seg = first_line_after(LBL_TITLE, page1, [LBL_EVENTDATE, re.compile(r"\n\s*5\.\s")], 600)
    if seg:
        t = oneline(seg)
        if t and len(t) > 5:
            r["title"] = t.strip(" |:-")
            q["title"] = "form"

    dm = DATE_ROW.search(page1)
    if dm:
        ev = iso(dm.group(1), dm.group(2), dm.group(3))
        rp = iso(dm.group(7), dm.group(8), dm.group(9)[:4])
        if ev:
            r["event_date"], q["event_date"] = ev, "form"
        if rp:
            r["report_date"], q["report_date"] = rp, "form"
        r["form_ler"] = norm_ler(dm.group(4), dm.group(5), dm.group(6))

    seg = first_line_after(LBL_MODE, page1, [], 120)
    if seg:
        mm = re.search(r"^[^\S\n]*([1-6])(?:\s*[-–]\s*[A-Za-z][A-Za-z ]+)?[^\S\n]*$", seg, re.M)
        if mm:
            r["operating_mode"], q["operating_mode"] = mm.group(0).strip(), "form"
    seg = first_line_after(LBL_POWER, page1, [], 400)
    if seg:
        pm = (re.search(r"^[^\S\n]*(\d{1,3})[^\S\n]*%?[^\S\n]*$", seg, re.M)
              or re.search(r"(?<![\d.(])(\d{1,3})\s*%", seg))
        if pm and int(pm.group(1)) <= 120:
            r["power_level"], q["power_level"] = int(pm.group(1)), "form"

    # box 2 (reporting unit's docket): first docket number between the facility and title labels
    tm = LBL_TITLE.search(page1)
    hdr = page1[fm.start() if fm else 0: tm.start() if tm else 1500]
    dk = find_dockets(hdr)
    if not dk and re.search(r"(?<!\d)05[02](?!\d)", hdr):
        # 2024+ form: "050"/"052" prefix is a checkbox, docket written as 5 digits, e.g. 00296
        m5 = re.search(r"(?<!\d)00(\d{3})(?!\d)", hdr)
        if m5:
            dk = [d for d in ("05000" + m5.group(1), "05200" + m5.group(1)) if d in UNITS][:1]
            if dk:
                q["docket_box2"] = "5-digit"
    if not dk and candidates and fm:
        # old forms OCR'd one digit per box, box borders often read as "1": "0 15 10 10 10 12 15 14"
        # only the few lines right after the facility label, and only dockets at the ADAMS site
        lines = [l for l in hdr[fm.end() - fm.start():].split("\n") if l.strip()][:3]
        line = re.sub(r"[^\d\n]", "", "\n".join(lines))
        for d in candidates:
            if re.search("1?".join(d[-3:]), line) and re.search("0.?5.?0.?0.?0", line):
                dk = [d]
                q["docket_box2"] = "boxed-digits"
                break
    if dk:
        r["form_docket"] = dk[0]
    # box 8 (other facilities involved): dockets up to the operating-mode label; [] = box found but none listed
    om = LBL_OTHERFAC.search(page1)
    if om:
        mm8 = LBL_MODE.search(page1, om.end())
        blk = page1[om.end(): mm8.start() if mm8 and mm8.start() - om.end() < 1500 else om.end() + 600]
        r["form_other_dockets"] = sorted(set(find_dockets(blk)) - {r.get("form_docket")})

    am = LBL_ABSTRACT.search(page1)
    if am:
        rest = page1[am.end():]
        e = ABSTRACT_END.search(rest)
        ab = clean(rest[: e.start()] if e else rest[:4000])
        if ab and len(ab) > 40:
            r["abstract"], q["abstract"] = ab, "form"

    b13 = LBL_BLOCK13.search(page1)
    if b13:
        b14 = LBL_BLOCK14.search(page1, b13.end())
        blk = page1[b13.end(): b14.start() if b14 else b13.end() + 1500]
        r["components"] = [g.groups() for g in COMP_ROW.finditer(blk)]

    narrative = form[page1_end.start():] if page1_end else None
    return r, q, narrative


# ------------------------------------------------------------------ narrative sections
SECTIONS = [
    ("sec_plant_id", r"PLANT\s+(?:AND\s+SYSTEMS?\s+)?IDENTIFICATION|PLANT\s+DESCRIPTION|BACKGROUND(?:\s+INFORMATION)?"),
    ("sec_condition_prior", r"(?:PLANT\s+|UNIT\s+|REACTOR\s+)?(?:OPERATING\s+)?CONDITIONS?\s+(?:PRIOR\s+TO|BEFORE)\s+(?:THE\s+)?(?:EVENT|DISCOVERY|OCCURRENCE)"
                            r"|INITIAL\s+(?:PLANT\s+)?CONDITIONS?|PLANT\s+(?:OPERATING\s+)?(?:CONDITIONS?|STATUS)|PLANT\s+STATUS"),
    ("sec_description", r"(?:DETAILED\s+)?DESCRIPTION\s+OF\s+(?:THE\s+)?(?:EVENTS?|OCCURRENCES?|CONDITIONS?|REPORTABLE\s+EVENT)|EVENT\s+DESCRIPTION|DESCRIPTION"),
    ("sec_cause", r"(?:THE\s+)?(?:ROOT\s+|APPARENT\s+|PROBABLE\s+|DIRECT\s+)?CAUSES?(?:\s+OF\s+(?:THE\s+)?(?:EVENTS?|OCCURRENCES?|CONDITIONS?|EACH\s+COMPONENT\b.*))?"
                  r"|EVENT\s+CAUSES?|CAUSAL\s+FACTORS?|ROOT\s+CAUSE\s+ANALYSIS|CAUSE\s+DETERMINATION"),
    ("sec_safety", r"(?:ASSESSMENT|ANALYSIS)\s+OF\s+(?:THE\s+)?SAFETY\s+(?:SIGNIFICANCE|CONSEQUENCES)(?:\s+AND\s+IMPLICATIONS)?"
                   r"|(?:ANALYSIS\s*/\s*|REPORTABILITY\s+ANALYSIS\s+AND\s+)?SAFETY\s+(?:SIGNIFICANCE|ASSESSMENT|ANALYSIS|CONSEQUENCES|IMPLICATIONS)(?:\s+AND\s+IMPLICATIONS)?(?:\s+OF\s+THE\s+EVENT)?"
                   r"|SIGNIFICANT\s+SAFETY\s+CONSEQUENCES(?:\s+AND\s+IMPLICATIONS)?|ANALYSIS\s+OF\s+(?:THE\s+)?EVENT|EVENT\s+ANALYSIS"),
    ("sec_corrective", r"(?:IMMEDIATE\s+|PLANNED\s+|COMPLETED\s+|ADDITIONAL\s+|FURTHER\s+|PROPOSED\s+)?CORRECTIVE\s+ACTIONS?(?:\(S\))?"
                       r"(?:\s+(?:COMPLETED|PLANNED|TAKEN|PROPOSED|PENDING|TO\s+PREVENT\s+RECURRENCE))?"
                       r"|ACTIONS?\s+TO\s+PREVENT\s+RECURRENCE|ACTIONS?\s+TAKEN|COMPLETED\s+ACTIONS?|PLANNED\s+ACTIONS?"),
    ("sec_previous", r"PREVIOUS\s+(?:SIMILAR\s+)?(?:EVENTS?|OCCURRENCES?)|SIMILAR\s+(?:EVENTS?|OCCURRENCES?)|PREVIOUS\s+(?:LERS?|OCCURRENCES)|PRIOR\s+(?:SIMILAR\s+)?EVENTS?"),
    ("sec_reportability", r"REPORTABILITY(?:\s+(?:ANALYSIS|DETERMINATION|EVALUATION))?|REASON\s+FOR\s+(?:THE\s+)?REPORT|REPORTING\s+REQUIREMENTS?|REPORTABLE\s+EVENT\s+CLASSIFICATION"),
    ("sec_component", r"COMPONENT\s+FAILURE\s+(?:INFORMATION|DATA|DESCRIPTION)|FAILED\s+COMPONENTS?(?:\s+(?:IDENTIFICATION|INFORMATION))?|COMPONENT\s+INFORMATION"),
]
ENUM = r"(?:(?:[A-Z]|[IVXl1]{1,4}|\d{1,2}(?:\.\d{1,2})*)\s*[.):]\s*|\(\s*(?:[A-Za-z]|\d{1,2})\s*\)\s*)?"
HEADER_RES = [(k, re.compile(r"^\s*" + ENUM + r"(?:" + p + r")\b\s*(?:([:.\-–])\s*(.*))?$", re.I)) for k, p in SECTIONS]


def split_sections(narr):
    found = collections.OrderedDict()
    cur, buf = None, []

    def flush():
        if cur and buf:
            txt = clean("\n".join(buf))
            if txt:
                found[cur] = (found.get(cur, "") + "\n\n" + txt).strip()

    for line in narr.splitlines():
        s = line.strip()
        hit = None
        if 3 <= len(s) <= 120:
            for k, rx in HEADER_RES:
                m = rx.match(s)
                if not m:
                    continue
                trailing = (m.group(2) or "").strip()
                head = s[: len(s) - len(trailing)].strip() if trailing else s
                if len(head) > 75:
                    continue
                letters = [c for c in head if c.isalpha()]
                upper = letters and sum(c.isupper() for c in letters) / len(letters) > 0.8
                enumerated = re.match(r"^\s*(?:[A-Z]|[IVXl1]{1,4}|\d{1,2}(?:\.\d{1,2})*)\s*[.):]|^\s*\(", s)
                # whole line is the header, or "Header: text", or an upper-case / numbered header
                if not trailing or m.group(1) == ":" or upper or enumerated:
                    hit = (k, trailing)
                    break
        if hit:
            flush()
            cur, buf = hit[0], ([hit[1]] if hit[1] else [])
        elif cur:
            buf.append(line)
    flush()
    return found


# ------------------------------------------------------------------ other extractions
CFR_RE = re.compile(
    r"(?<![\d.])(50\.73|50\.72|50\.36|50\.46|50\.69|20\.2201|20\.2203|73\.71|73\.77|73\.1200|21\.21|50\.9)"
    r"(?:,?\s*(?:paragraph|section)\s*)?((?:\s*\(\s*[a-z0-9]{1,5}\s*\)){1,5})", re.I)


# A box mark left of a criterion that the OCR read as unchecked (the form prints q / o / D / a box).
CFR_UNCHECKED = re.compile(r"(?:^|[\s|])(?:q|o|O|0|D|\u2751|\u25a1|\[\s*\])\s*$")
CFR_LIST_GAP = 60      # chars between citations in the form's printed list of every criterion
CFR_LIST_MIN = 6       # that many in a row = the printed list, not the licensee's own words


def cfr_sections(text):
    """Reporting criteria the licensee cites. The Form 366 page prints every criterion with a
    checkbox; electronic forms carry no mark for which box is checked, so the printed list
    (CFR_LIST_MIN or more citations packed within CFR_LIST_GAP chars) is dropped whole, as is
    any citation after an unchecked-box mark. The abstract and narrative name the criteria in
    sentences, which are kept."""
    text = text or ""
    ms = list(CFR_RE.finditer(text))
    runs, run = [], []
    for m in ms:
        if run and m.start() - run[-1].end() > CFR_LIST_GAP:
            runs.append(run)
            run = []
        run.append(m)
    if run:
        runs.append(run)
    out = set()
    for run in runs:
        if len(run) >= CFR_LIST_MIN:
            continue
        for m in run:
            if CFR_UNCHECKED.search(text[max(0, m.start() - 6):m.start()]):
                continue
            out.add(m.group(1) + re.sub(r"\s+", "", m.group(2)))
    return out



# ------------------------------------------------------------------ abstract cleanup
# Fillable forms (2000s on) put every field value after the ABSTRACT label in the PDF text; OCR'd
# forms interleave the abstract with page-1 blocks; and without an end marker the abstract runs
# into page 2. trim_abstract() keeps the licensee's prose and drops the rest.

# Page 2 starts (or the abstract's continuation sheet): the abstract ends here.
PAGE_END = re.compile(
    r"U\.\s*S\.\s*NUCLEAR\s*REGULATORY\s*COMMISSION|APPROVED\s*BY\s*OMB|^\W*N\w{0,2}[CG]\s*FORM\s*36|CONTINUATION\s*SHEET"
    r"|^\s*LICENSEE\s*EVENT\s*REPORT\s*\(LER\)\s*(?:\(Continued\))?\s*$"
    r"|(?i:Estimated\s*burden\s*per\s*response|Paperwork\s*Reduction|^\s*Page\s*\d+\s*of\s*\d+\s*$|https?://|mailto:"
    r"|Docusign\s*Envelope|\(If\s*more\s*space|^\s*[A-Z]:\\\S)", re.M)
# Page-1 blocks 11-15 printed between parts of the abstract: skipped, and the abstract resumes after.
# Uppercase-only where prose could use the same words.
FORM_LABEL = re.compile(
    r"SUBMITTED\s*PURSUANT\s*TO\s*THE\s*REQUIREMENTS|(?i:Check\s*all\s*that\s*apply|Supplemental\s*Report\s*Expected"
    r"|Expected\s*Submission|Specify\s*in\s*Abstract\s*below)")
PHONE = re.compile(r"\(?\d{3}\)?[\s.-]*\d{3}\s*-\s*\d{4}|(?<!\d)\d{10}(?!\d)")
MARKS = re.compile("[✔✓■□☐☒]")
DOCKET_LINE = re.compile(r"^\s*\d{3,5}\s+\d\s*$")     # fillable form: docket and page count under the facility name
FULL_DOCKET = re.compile(r"(?<!\d)05[02]00\d{3}(?!\d)")
DOCKET_TAIL = re.compile(r"\s\d{3,5}\s+\d\s*$")
TERMINAL = re.compile(r"[.!?][\"')\]]?\s*$")
WORD3 = re.compile(r"[A-Za-z]{3,}")
FORM_WORDS = set("""cause system component manufacturer manu facturer reportable to iris epix ices nprds month day
year date expected submission supplemental report facility name docket number page title complete one line
for each failure described in this the yes no if licensee contact telephone include area code sequential
revision rev no event ler other facilities involved operating mode power level or""".split())


def cfr_block_spans(text):
    """(start, end) of each printed criteria list: CFR_LIST_MIN+ citations packed together with
    nothing but boxes between them. A licensee's own list ("1) 10 CFR 50.73(a)..., 2) 10 CFR ...")
    repeats "CFR" between citations, so it is kept."""
    spans, run = [], []
    def close():
        gaps = [text[a.end():b.start()] for a, b in zip(run, run[1:])]
        if len(run) >= CFR_LIST_MIN and sum("CFR" not in g.upper() for g in gaps) >= len(gaps) / 2:
            spans.append((run[0].start(), run[-1].end()))
    for m in CFR_RE.finditer(text):
        if run and m.start() - run[-1].end() > CFR_LIST_GAP:
            close()
            run = []
        run.append(m)
    if run:
        close()
    return spans


def norm(s):
    return re.sub(r"\W+", " ", s or "").strip().lower()


def same_as_title(s, title):
    a, b = norm(s), norm(title)
    if not a or not b:
        return False
    # a wrapped title line is part of the title; a line is the title plus at most a short suffix ("- Rev. 1")
    return a == b or (len(a) >= 25 and a in b) or (len(b) >= 25 and b in a and len(a) <= len(b) + 15)


def hard_junk(line, title):
    """Never abstract prose: blank, checkbox marks, the title again, form labels, a contact phone, number rows."""
    s = MARKS.sub("", line).strip()
    if not s or same_as_title(s, title):
        return True
    words = re.findall(r"[a-z]+", s.lower())
    if words and sum(w in FORM_WORDS for w in words) >= 0.8 * len(words):
        return True
    if PHONE.search(s) and len(s) < 90:
        return True
    if FULL_DOCKET.search(s) and len(s) < 100:          # page header row: facility, docket, LER number, page
        return True
    letters = [ch for ch in s if ch.isalpha()]
    if len(letters) < 0.5 * len(s.replace(" ", "")) and len(WORD3.findall(s)) < 3:   # dates, numbers, OCR specks
        return True
    return bool(letters) and sum(ch.islower() for ch in letters) < 0.25 * len(letters)   # ALL-CAPS labels


def label_line(line):
    """A form label ("Month Day Year", "13. Complete One Line for each Component ...") or a facility
    or signer name: Title Case, no sentence punctuation, short unless numbered like a form block."""
    bare = MARKS.sub("", line).strip()
    s = re.sub(r"^\d{1,2}\.\s*", "", bare)
    words = [w for w in s.split() if w[0].isalpha()]
    if not words or re.search(r"[,;:]\s|\.\s+[A-Z]|[.;]$", s):
        return False
    if len(s) >= 60 and s == bare:
        return False
    return sum(w[0].isupper() for w in words) >= 0.6 * len(words)


def form_line(lines, i, title):
    """Line i is form text, not abstract prose (used before the abstract starts and between its parts)."""
    l = lines[i]
    return bool(hard_junk(l, title) or label_line(l) or FORM_LABEL.search(l) or "■" in l or DOCKET_TAIL.search(l)
                or (i + 1 < len(lines) and DOCKET_LINE.match(lines[i + 1])))


def squash(s):
    return re.sub(r"\s+", "", s)


def repeats_start(lines, j, start, first):
    """Line j begins a second copy of the abstract (two text layers in one PDF): 150 chars match."""
    return (len(first) >= 40 and first in lines[j]
            and squash(lines[j][lines[j].index(first):] + "\n" + "\n".join(lines[j + 1:]))[:150] == start)


def drop_junk_paragraphs(text, title):
    """Paragraphs of only form text or stray tokens; at the end also short names ("Bernadette Humphrey").
    Whole paragraphs only, so the last line of a sentence ("2009.") stays."""
    paras = [p.split("\n") for p in re.split(r"\n\s*\n", text)]
    def stray(l):
        return hard_junk(l, title) or (len(l.split()) <= 1 and len(l.strip()) < 25)   # "jbrow34", "Line"
    while paras and all(stray(l) or label_line(l) for l in paras[-1]):
        paras.pop()
    return "\n\n".join("\n".join(p) for p in paras if not all(stray(l) for l in p))


def trim_abstract(ab, title=None):
    """The abstract without Form 366 labels and field values around and inside it.
    Returns (text, None) unchanged, (text, how) trimmed, or (None, "no_prose") if nothing is left."""
    if not ab:
        return ab, None
    if title and len(title) > 200:      # a form title that ran into the next fields: useless for matching
        title = None
    how, original = set(), ab
    # the printed criteria list is blanked out first (same length, so offsets hold); it can sit mid-line
    for a, b in cfr_block_spans(ab):
        ab = ab[:a] + "\n" + " " * (b - a - 1) + ab[b:]
        how.add("cfr_list")
    lines = ab.split("\n")
    i = 0
    while i < len(lines) and (form_line(lines, i, title) or PAGE_END.search(lines[i])):
        i += 1
    if i:
        how.add("head")
    body, skipping = [], False
    start, first = squash("\n".join(lines[i:]))[:150], (MARKS.sub("", lines[i]).strip()[:60] if i < len(lines) else "")
    for j in range(i, len(lines)):
        l = lines[j]
        end = PAGE_END.search(l)
        if end or (j > i and repeats_start(lines, j, start, first)):
            cut = end.start() if end else l.index(first)
            if not skipping and l[:cut].strip():
                body.append(l[:cut])
            how.add("tail")
            break
        if skipping:
            if form_line(lines, j, title):
                continue
            skipping = False
            body.append("")
        label = FORM_LABEL.search(l)
        if label:
            body.append(l[:label.start()])
            skipping = True
            how.add("mid")
            continue
        body.append(l)
    out = clean(drop_junk_paragraphs("\n".join(body), title))
    if not out or len(out) <= 40:
        return None, "no_prose"
    if out == clean(original):
        return original, None
    return out, "+".join(sorted(how)) or "paras"


def mode_power_from_text(text):
    mode = power = None
    m = re.search(r"\bMode\s*:?\s*([1-6])\b", text or "")
    if m:
        mode = m.group(1)
    p = (re.search(r"(?:Reactor\s+)?Power(?:\s+Level)?\s*:\s*(\d{1,3})(?![\d.])", text or "", re.I)
         or re.search(r"(?<![\d.])(\d{1,3})(?:\.\d+)?\s*(?:%|percent)\s*(?:of\s+)?(?:rated\s+thermal\s+|reactor\s+|rated\s+)?power",
                      text or "", re.I))
    if p and int(p.group(1)) <= 120:
        power = int(p.group(1))
    return mode, power


def facility_from_title(title):
    m = re.search(r"\bLER[^,]*?\bfor\s+(.+?)(?:,?\s+(?:Regarding|re|Re:|RE)\b|,\s+(?![Uu]nits?\b)|$)", title or "")
    if not m:
        return None
    f = m.group(1).strip(" ,.-")
    um = re.match(r"(.+?,?\s+Units?\s+(?:No\.?\s*)?[\dA-Za-z]+(?:\s*(?:&|and|,)\s*\d)*)", f)
    return (um.group(1) if um else f).strip(" ,") or None


def title_from_meta(title):
    t = title or ""
    m = re.search(r"\b(?:Regarding|re:?)\s+(.+)$", t)
    if m:
        return m.group(1).strip(" .")
    fac = facility_from_title(t)
    if fac and fac in t:
        return t.split(fac, 1)[1].strip(" ,.-") or None
    return None


# ------------------------------------------------------------------ title cleanup
# The form title is read from "4. Title" to the next label; when OCR damages or the fillable-form
# layout hides that label, the title runs on into blocks 5-15. clean_title() cuts it back.
TITLE_LABELS = r"(?:Event\s*Date|LER\s*Number|Report\s*Date|Other\s*Facilit|Operating\s*Mode|Power\s*Level)"
TITLE_END = re.compile(
    r"(?i:\s[5-9S]\s*[.,]?\s*" + TITLE_LABELS + r")"              # numbered label, any case: "6. Event Date", "S. EVENT DATE"
    r"|\s(?:E\W?V\W?E\W?\s?N?\W?I?\W?T\W*\s*(?:D\W?A|\(5\)|$)|LER\s*N|REPORT\s*D\W?ATE|OTHER\s*FACIL|OPERATING\s*MODE"
    r"|POWER\s*LEVEL|FACILITY\s*NAME|DOCKET\s*NUMBER|SEQUENTIAL|MONTH\s+DAY|PAGE\s*\(3\))"   # uppercase, OCR-damaged
    r"|(?i:\s3\s*[.,]\s*PAGE\b|Specify\s*in\s*Abstract|Licensee\s*Contact|\bAbstract\s*\(Limit"
    r"|\bTHIS\s+REPORT\s+IS\s+SUBMITTED|\bE\w{4,8}\s*burd\w*\s*per|burden\s*per\s*response|APPROVED\s*BY\s*[O0]MB"
    r"|COMPLETE\s*ONE\s*LINE|LICENSEE\s*EVENT\s*REPORT|\(See\s*reverse|Limit\s*to\s*\d{4}\s*spaces)"
    r"|\s(?:0[1-9]|1[0-2])\s+[0-3]\d\s+(?:19|20)?\d\d(?!\d)")       # the event-date row: 03 18 2023
TITLE_LEFTOVER = re.compile(r"(?:\s+(?:[5-9S][.,]|[YI|\u2022\u25a1\u25a0]|\(\d\)))+\s*$")   # label number or box at the end
NOT_A_TITLE = re.compile(r"^\W*(?:APPROVED\s*BY\s*[O0]MB|Estimated\s*burden|U\.\s*S\.\s*NUCLEAR|NRC\s*FORM"
                         r"|LICENSEE\s*EVENT\s*REPORT)", re.I)
OCR_LABEL_HINT = re.compile(r"\(\s*[5-8I]\s*[)I1l]|\bDA\w{0,2}\b|\bEV\w{0,4}\b|NUMB|FACIL|INVOLV|[~\u2022\\{}]", re.I)
CAPS_WORD = re.compile(r"[A-Z]{3,}\W*")


def garbage_token(w):
    """Form rules and OCR specks ("._\u2022en_.+", "...."); not curly quotes, "+/-" or OCR "~" for "i"."""
    return bool(re.search(r"[_\u2022|\\{}]{2,}|[._\u2022]{4,}|_.*[\u2022+]|[\u2022+].*_", w))


def trim_to_adams_title(t, adams_title):
    """Cut a form title where it stops matching the ADAMS title, if what follows looks like
    OCR-damaged labels ("... Water Level Transient ... Isolation EVENT DAT (5)")."""
    words, m = t.split(), " %s " % norm(adams_title)
    for k in range(len(words) - 1, 3, -1):
        head = norm(" ".join(words[:k]))
        if head and " %s " % head in m:
            return " ".join(words[:k]) if OCR_LABEL_HINT.search(" ".join(words[k:])) else t
    return t


def trim_ocr_tail(t):
    """A mixed-case title followed by OCR-damaged labels: cut at the first punctuation-heavy token, or
    at the first of 3-in-4 all-caps words ("Substation FVFNIT nATF() I FR NIIMRFR")."""
    words = t.split()
    if len(words) < 6 or sum(w.isupper() for w in words[:4]) > 1:
        return t
    for i in range(4, len(words)):
        if garbage_token(words[i]):
            return " ".join(words[:i])
        caps = [j for j in range(i, min(i + 4, len(words))) if CAPS_WORD.fullmatch(words[j])]
        if len(caps) >= 3:
            return " ".join(words[:caps[0]])
    return t


def clean_title(t, adams_title=None):
    """The form title without the form labels and field values it ran into; None if it is not a title."""
    if not t or NOT_A_TITLE.search(t):
        return None
    t = re.sub(r"_{3,}", " ", t)
    m = TITLE_END.search(" " + t)          # leading space: a label at the very start leaves nothing
    if m:
        t = t[:max(0, m.start() - 1)]
    if adams_title:
        t = trim_to_adams_title(t, adams_title)
    t = TITLE_LEFTOVER.sub("", trim_ocr_tail(t)).strip(" |:;,-")
    return t if len(t) > 5 else None


# ------------------------------------------------------------------ per-document
EVENT_ON = re.compile(r"\b(?:On|At\s+\S+\s+(?:hours\s+)?on)\s+((?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\.?"
                      r"\s+\d{1,2},?\s+(?:19|20)\d{2})")
# Two-digit month and day as printed on the form; a single digit is usually OCR debris ("07 1 05 1 2012").
MDY = re.compile(r"(?<!\d)(0[1-9]|1[0-2])\s+(0[1-9]|[12]\d|3[01])\s+((?:19|20)\d{2})(?!\d)")


def text_event_date(text):
    """First "On March 26, 2003" / "At 1731 hours on December 19, 2012" in text, as ISO."""
    m = EVENT_ON.search(text or "")
    if not m:
        return None
    for fmt in ("%B %d, %Y", "%B %d %Y", "%b %d, %Y", "%b. %d, %Y", "%b %d %Y"):
        try:
            return dt.datetime.strptime(re.sub(r"\s+", " ", m.group(1)), fmt).date().isoformat()
        except ValueError:
            pass
    return None


def loose_form_event_date(content):
    """First month/day/year after the EVENT DATE label, for forms whose date row is too damaged
    for DATE_ROW (e.g. "05 11 2003 2003 - 004 - RV 07 10 2003")."""
    m = LBL_EVENTDATE.search(content or "") or re.search(r"EVENT\s+DATE", content or "", re.I)
    if not m:
        return None
    for mm in MDY.finditer(content[m.end(): m.end() + 700]):
        d = iso(*mm.groups())
        if d:
            return d
    return None


def plausible_event_date(ev, report_date):
    """On or before the report date, and no more than four years earlier."""
    if not report_date:
        return True
    try:
        e, r = dt.date.fromisoformat(ev), dt.date.fromisoformat(report_date[:10])
    except ValueError:
        return False
    return dt.timedelta(days=-1) <= r - e <= dt.timedelta(days=4 * 366)


def parse_doc(row):
    acc, title, dtype, ddate, added, docket_j, rep_j, author_j, url, content = row
    content = content or ""
    meta_title = title or ""
    q = {}
    rec = {"accession_number": acc, "document_date": ddate, "date_added": (added or "")[:10] or None,
           "pdf_url": url, "content_chars": len(content), "low_text": int(len(content) < 2000)}
    dockets = json.loads(docket_j) if docket_j else []
    adams = []
    for d in dockets:
        m = re.fullmatch(DOCKET_RE, (d or "").strip())
        d8 = norm_docket(m) if m else None
        if m and not d8 and re.fullmatch(r"0?5000\d{3}", d):
            d8 = d.zfill(8)          # well-formed but not in plants_ref: keep it, flagged later
        if d8 and d8 not in adams:
            adams.append(d8)
    rec["docket"] = adams[0] if adams else None
    # non-reactor dockets: spent fuel storage at a reactor site (used only when no reactor docket),
    # and the docket the event key falls back to (materials licenses 030-, source material 040-, ...)
    raw_dockets = [d.strip() for d in dockets if re.fullmatch(r"\d{8}", (d or "").strip())]
    isfsi = [d for d in raw_dockets if UNITS.get(d, (None,) * 3)[2] == "ISFSI"]
    other_dockets = [d for d in raw_dockets if d not in adams and d != PLACEHOLDER_DOCKET]
    authors = json.loads(author_j) if author_j else []
    rec["licensee"] = "; ".join(a for a in authors if a) or None

    rep_nums = ler_numbers_in(" ".join(json.loads(rep_j)) if rep_j else "")
    title_nums = ler_numbers_in(meta_title)

    cands = sorted({d for a in adams if a in UNITS for d, u in UNITS.items() if u[0] == UNITS[a][0]})
    form, fq, narrative = parse_form(content, cands)
    q.update(fq)
    # reporting unit: form box 2 wins over the first ADAMS docket (ADAMS order is not meaningful)
    if form.get("form_docket"):
        if rec["docket"] and form["form_docket"] != rec["docket"]:
            q["docket_adams_first"] = rec["docket"]
        rec["docket"], q["docket"] = form["form_docket"], "form"
    elif rec["docket"]:
        q["docket"] = "meta"
    elif isfsi:
        rec["docket"], q["docket"] = isfsi[0], "meta_isfsi"
        adams.append(isfsi[0])

    rec["key_docket"] = event_key_docket(rec["docket"], other_dockets, acc)

    rec["facility"] = form.get("facility") or facility_from_title(meta_title)
    if "facility" not in q and rec["facility"]:
        q["facility"] = "meta"
    form_title = clean_title(form.get("title"), meta_title)
    if form.get("title") and form_title != form.get("title"):
        q["title_trimmed"] = 1 if form_title else "not_a_title"
    if not form_title:
        q.pop("title", None)
    rec["title"] = form_title or title_from_meta(meta_title) or meta_title or None
    q.setdefault("title", "meta")
    rec["event_date"] = form.get("event_date")
    rec["report_date"] = form.get("report_date") or ddate
    if form.get("report_date") and ddate:
        try:
            gap = abs((dt.date.fromisoformat(form["report_date"]) - dt.date.fromisoformat(ddate[:10])).days)
        except ValueError:
            gap = 0
        if gap > 400:  # OCR-damaged year on the form; trust ADAMS
            q["report_date_rejected"] = form["report_date"]
            rec["report_date"] = ddate
            q["report_date"] = "meta"
    if "report_date" not in q:
        q["report_date"] = "meta"
    rec["form_type"] = form.get("form_type", "pre-366/other")
    if re.search(r"CANCEL\w*\s+(?:OF\s+)?(?:LICENSEE\s+EVENT\s+REPORT|LER)|(?:cancel|withdraw)\w*\s+(?:\(\w+\s+\w+\)\s+)?"
                 r"(?:of\s+)?(?:Licensee\s+Event\s+Report|LER)", content[:6000], re.I):
        rec["form_type"] = "cancellation"
    rec["abstract"] = form.get("abstract")

    narr = clean(strip_boilerplate(narrative)) if narrative else None
    if narr is None and rec["form_type"] == "pre-366/other" and content:
        narr = clean(content)  # letters / special reports: keep the whole text as narrative
    rec["narrative"] = narr
    secs = split_sections(narr) if narr else {}
    for k, _ in SECTIONS:
        rec[k] = secs.get(k)
    q["sections"] = sorted(secs)

    if rec["abstract"]:
        rec["abstract"], how = trim_abstract(rec["abstract"], rec.get("title"))
        if how:
            q["abstract_trimmed"] = how
        if not rec["abstract"]:
            q["abstract"] = "form_labels_only"     # replaced below if the text has an abstract
    if not rec["abstract"]:
        m = re.search(r"^\s*(?:\d+\.\s*)?ABSTRACT\b[^\n]*\n(.{80,3000}?)(?:\n\s*\n|NRC\s+FORM|\Z)", content, re.M | re.S)
        if m:
            rec["abstract"], how = trim_abstract(clean(m.group(1)), rec.get("title"))
            if rec["abstract"]:
                q["abstract"] = "text"
                if how:
                    q["abstract_trimmed"] = how

    body = "\n".join(x for x in (rec["abstract"], narr) if x)
    # condition-prior section is the most reliable; then the form; then anywhere in the text
    cm, cp = mode_power_from_text(rec.get("sec_condition_prior"))
    tm, tp = mode_power_from_text(body)
    for col, vals in (("operating_mode", (cm, form.get("operating_mode"), tm)),
                      ("power_level", (cp, form.get("power_level"), tp))):
        rec[col] = next((v for v in vals if v is not None), None)
        src_ = ["section", "form", "text"][[v is not None for v in vals].index(True)] if rec[col] is not None else None
        q.pop(col, None)
        if src_:
            q[col] = src_
    if rec["event_date"] is None:
        ev = text_event_date(body)
        if ev:
            rec["event_date"], q["event_date"] = ev, "text"
    # Fallbacks for damaged forms and unsplit text; kept only when plausible for the report date.
    if rec["event_date"] is None:
        for src_, ev in (("form_loose", loose_form_event_date(content)), ("text_raw", text_event_date(content[:20000]))):
            if ev and plausible_event_date(ev, rec["report_date"]):
                rec["event_date"], q["event_date"] = ev, src_
                break

    # LER number: after the event date, which breaks ties between title and report-number field
    text_nums = ler_numbers_in(content[:5000])
    primary, src_, rejected = choose_ler(form.get("form_ler"), rep_nums, title_nums, rec["event_date"],
                                         cited_numbers(content[:5000]), ddate)
    if not primary and text_nums:
        primary, src_ = text_nums[0], "text"
    if src_:
        q["ler_number"] = src_
    if rejected:
        q["ler_number_rejected"] = rejected
    # other numbers the document bundles (a source listing several), minus the ones just rejected
    nums = [primary] if primary else []
    for group in (rep_nums, title_nums):
        if len(group) > 1:
            nums += [n for n in group if n not in nums and n not in rejected]
    rec["ler_number"] = primary
    if primary:
        y, s, r = primary.split("-")
        rec.update(ler_year=int(y), ler_seq=int(s), ler_rev=int(r))
        rec["ler_key"] = "%s:%s-%s" % (rec["key_docket"], y, s)

    rec["site_id"], _, rec["reactor_type"], rec["reactor_vendor"] = UNITS.get(rec.get("docket"), (None, None, None, None))

    # every unit the document names, with where each one came from
    units = collections.OrderedDict()
    def add(d, flag):
        units.setdefault(d, {"in_adams": 0, "in_form_box2": 0, "in_form_box8": 0, "in_title": 0})[flag] = 1
    for d in adams:
        add(d, "in_adams")
    if form.get("form_docket"):
        add(form["form_docket"], "in_form_box2")
    for d in form.get("form_other_dockets") or []:
        add(d, "in_form_box8")
    for d in (title_dockets(meta_title, rec["site_id"]) + title_dockets(rec.get("title"), rec["site_id"])
              + title_dockets(rec.get("facility"), rec["site_id"])):
        add(d, "in_title")
    form_read = "form_docket" in form and "form_other_dockets" in form
    ler_units = []
    for d, f in units.items():
        if d == rec["docket"]:
            role = "reporting"
        else:
            role = "other"
        if form_read:   # the licensee filled in boxes 2 and 8: trust them (plus "Units 1 and 2" titles)
            affected = int(d == rec["docket"] or f["in_form_box8"] or f["in_title"])
        else:           # no readable form header: fall back to ADAMS docketing
            affected = 1
        ler_units.append((acc, d, UNITS.get(d, (None,))[0], role, f["in_adams"], f["in_form_box2"],
                          f["in_form_box8"], f["in_title"], affected, "form" if form_read else "adams"))

    # sanity: event before report, not before 1960
    if rec["event_date"] and (rec["event_date"] < "1960" or (rec["report_date"] and rec["event_date"] > rec["report_date"])):
        q["event_date_rejected"] = rec["event_date"]
        rec["event_date"] = None
        q.pop("event_date", None)

    rec["parse_quality"] = json.dumps(q, sort_keys=True)
    comps = form.get("components") or []
    cfrs = cfr_sections(body)
    return rec, nums, comps, cfrs, ler_units


LER_COLS = ["accession_number", "ler_number", "ler_year", "ler_seq", "ler_rev", "ler_key", "docket", "site_id",
            "facility", "licensee", "title", "event_date", "report_date", "document_date", "date_added", "operating_mode",
            "power_level", "form_type", "abstract", "narrative"] + [k for k, _ in SECTIONS] + [
            "reactor_type", "reactor_vendor", "content_chars", "low_text", "pdf_url", "parse_quality"]


# ADAMS files some NRC Accident Sequence Precursor analyses under the LER type; they cite the LER
# number, so parsed as LERs they become its "latest revision" and hide the licensee's report.
NOT_LER_TITLE = re.compile(r"precursor analys|\bASP\s+(screening\s+)?analys|accident sequence precursor", re.I)


def build(raw, out):
    src = sqlite3.connect("file:%s?mode=ro" % os.path.abspath(raw), uri=True)
    tmp = out + ".tmp"
    if os.path.exists(tmp):
        os.remove(tmp)
    dst = sqlite3.connect(tmp)
    dst.executescript(SCHEMA)
    n = 0
    rows = src.execute("SELECT accession_number, document_title, document_type, document_date, date_added, "
                       "docket_number, report_number, author_affiliation, url, content FROM adams_documents")
    skipped = 0
    for row in rows:
        if NOT_LER_TITLE.search(row[1] or ""):
            skipped += 1          # NRC precursor analysis filed as an LER; it would shadow the real LER
            continue
        rec, nums, comps, cfrs, lus = parse_doc(row)
        dst.executemany("INSERT INTO ler_units VALUES (?,?,?,?,?,?,?,?,?,?)", lus)
        dst.execute("INSERT INTO lers (%s) VALUES (%s)" % (",".join(LER_COLS), ",".join("?" * len(LER_COLS))),
                    [rec.get(c) for c in LER_COLS])
        for i, num in enumerate(nums):
            y, s, _ = num.split("-")
            dst.execute("INSERT OR IGNORE INTO ler_numbers VALUES (?,?,?,?)",
                        (rec["accession_number"], num, "%s:%s-%s" % (rec["key_docket"], y, s), int(i == 0)))
        for i, c in enumerate(comps, 1):
            dst.execute("INSERT INTO ler_components VALUES (?,?,?,?,?,?,?)", (rec["accession_number"], i, *c))
        for c in cfrs:
            dst.execute("INSERT INTO ler_cfr VALUES (?,?)", (rec["accession_number"], c))
        n += 1
        if n % 2000 == 0:
            print("  parsed %d" % n, flush=True)
    dst.executemany("INSERT INTO sites VALUES (?,?,?)", [(k,) + v for k, v in sorted(SITES.items())])
    dst.executemany("INSERT INTO site_aliases VALUES (?,?)", [(k, a) for k, v in sorted(ALIASES.items()) for a in v])
    dst.executemany("INSERT INTO units VALUES (?,?,?,?,?,?,?,?,?,?)", list(unit_rows()))
    dst.executescript(POST_SCHEMA)
    dst.executemany("INSERT INTO meta VALUES (?,?)", [
        ("parser_version", PARSER_VERSION), ("built_at", dt.datetime.now().isoformat(timespec="seconds")),
        ("raw_db", os.path.abspath(raw)), ("raw_rows", str(n))])
    dst.commit()
    dst.close()
    os.replace(tmp, out)
    print("built %s: %d documents" % (out, n))


COVERAGE_FIELDS = ["ler_number", "docket", "facility", "title", "event_date", "report_date", "operating_mode",
                   "power_level", "abstract", "narrative", "sec_description", "sec_cause", "sec_corrective",
                   "sec_safety", "sec_previous", "reactor_type"]


def coverage(out):
    con = sqlite3.connect(out)
    decades = [r[0] for r in con.execute("SELECT DISTINCT substr(document_date,1,3)||'0s' FROM lers ORDER BY 1")]
    print("%-16s" % "", *["%6s" % d for d in decades], "%6s" % "all")
    tot = dict(con.execute("SELECT substr(document_date,1,3)||'0s', COUNT(*) FROM lers GROUP BY 1"))
    print("%-16s" % "n", *["%6d" % tot[d] for d in decades], "%6d" % sum(tot.values()))
    for f in COVERAGE_FIELDS:
        got = dict(con.execute("SELECT substr(document_date,1,3)||'0s', COUNT(%s) FROM lers GROUP BY 1" % f))
        print("%-16s" % f, *["%5d%%" % (100 * got.get(d, 0) / tot[d]) for d in decades],
              "%5d%%" % (100 * sum(got.values()) / sum(tot.values())))
    for label, sql in (("components", "ler_components"), ("cfr cited", "ler_cfr")):
        got = dict(con.execute("SELECT substr(l.document_date,1,3)||'0s', COUNT(DISTINCT x.accession_number) "
                               "FROM %s x JOIN lers l USING(accession_number) GROUP BY 1" % sql))
        print("%-16s" % label, *["%5d%%" % (100 * got.get(d, 0) / tot[d]) for d in decades],
              "%5d%%" % (100 * sum(got.values()) / sum(tot.values())))


def show(raw, acc):
    src = sqlite3.connect("file:%s?mode=ro" % os.path.abspath(raw), uri=True)
    row = src.execute("SELECT accession_number, document_title, document_type, document_date, date_added, "
                      "docket_number, report_number, author_affiliation, url, content FROM adams_documents "
                      "WHERE accession_number=?", (acc,)).fetchone()
    if not row:
        sys.exit("not found")
    rec, nums, comps, cfrs, lus = parse_doc(row)
    for k in LER_COLS:
        v = rec.get(k)
        if isinstance(v, str) and len(v) > 300:
            v = v[:300].replace("\n", " ") + " ...[%d chars]" % len(v)
        print("%-20s %s" % (k, v))
    print("ler_numbers         ", nums)
    print("components          ", comps)
    print("cfr                 ", sorted(cfrs))
    print("units (docket, role, adams, box2, box8, title, affected, basis):")
    for u in lus:
        print("   ", u[1], *u[3:])


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--raw", default="lers.db")
    ap.add_argument("--out", default="lers_parsed.db")
    ap.add_argument("--show")
    ap.add_argument("--coverage", action="store_true")
    a = ap.parse_args()
    if a.show:
        show(a.raw, a.show)
    elif a.coverage:
        coverage(a.out)
    else:
        build(a.raw, a.out)
        coverage(a.out)


if __name__ == "__main__":
    main()
