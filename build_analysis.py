#!/usr/bin/env python3
"""Combine lers_parsed.db + lers_classified.db (+ ADAMS titles from lers.db) into lers_analysis.db.

Read-only on all three inputs; the output is deleted and rebuilt on every run, so it can be
regenerated after a re-parse or a new classification run.

  python3 build_analysis.py [--prompt-version p4] [--model claude-haiku-5-5] [--release-version r2]

lers_analysis.db = every table/view of lers_parsed.db, plus
  adams_titles      accession -> clean ADAMS index title
  classification    one row per classified event, every scalar field as a column
  class_generation  per-unit generation impact (one row per unit per event), mapped to dockets
  class_tags        multi-valued fields (who, operator_fundamentals, safety_functions_lost, ...)
                    The environment fields (radiological_release, environment_evidence, non_radiological tags)
                    come from the classify_release.py second pass where it ran (release_check column says
                    which), since p4's own release calls are loose.
  events            view: latest LER revision + site/unit + classification, the main query surface
"""
import argparse
import json
import os
import re
import sqlite3

HERE = os.path.dirname(os.path.abspath(__file__))
PARSED, CLASSIFIED, RAW = (os.path.join(HERE, f) for f in ("lers_parsed.db", "lers_classified.db", "lers.db"))
ASP = os.path.join(HERE, "asp_parsed.db")       # optional: NRC precursor results (parse_asp.py)
OUT = os.path.join(HERE, "lers_analysis.db")

SCHEMA = """
CREATE TABLE adams_titles (accession_number TEXT PRIMARY KEY, title TEXT);
CREATE TABLE classification (
    accession_number TEXT PRIMARY KEY, prompt_version TEXT, model TEXT, classified_at TEXT,
    summary TEXT,
    primary_cause TEXT, human_error TEXT, error_type TEXT, cause_evidence TEXT,
    eiis_code TEXT, system_name TEXT, component TEXT, failure_mode TEXT, failure_mechanism TEXT,
    fuel_cladding TEXT, rcs_pressure_boundary TEXT, containment TEXT, barrier_evidence TEXT,
    radiological_release TEXT, environment_evidence TEXT,
    trip_risk TEXT, ts_required_shutdown TEXT, stated_cost_usd REAL, risk_metrics TEXT,
    confidence TEXT, notes TEXT,
    result_json TEXT,             -- the full model output, as returned
    release_check TEXT,           -- classify_release.py version that set the environment fields, else NULL (p4's)
    release_pathway TEXT, non_radiological_description TEXT
);
CREATE TABLE class_generation (
    accession_number TEXT, unit_name TEXT, docket TEXT, event_occurred_here INTEGER, impact TEXT,
    power_before_pct INTEGER, power_after_pct INTEGER, duration_hours REAL, duration_evidence TEXT
);
CREATE TABLE class_tags (accession_number TEXT, field TEXT, value TEXT);
CREATE TABLE asp_precursors (     -- NRC Accident Sequence Precursor results, one row per LER
    docket TEXT, ler_year INTEGER, ler_seq INTEGER, ler_key TEXT PRIMARY KEY,
    value REAL, finding TEXT, significant INTEGER, event_date TEXT, plant TEXT, description TEXT,
    n_sources INTEGER, n_agree INTEGER, values_seen TEXT, sources TEXT);
"""

POST = """
CREATE INDEX idx_cg_acc ON class_generation(accession_number);
CREATE INDEX idx_cg_docket ON class_generation(docket, impact);
CREATE INDEX idx_tags ON class_tags(field, value);
CREATE INDEX idx_tags_acc ON class_tags(accession_number);
CREATE INDEX idx_cls_cause ON classification(primary_cause, human_error);
CREATE INDEX idx_cls_sys ON classification(eiis_code, failure_mode);
CREATE VIEW events AS
  SELECT l.accession_number, l.ler_number, l.ler_key, l.event_date, l.report_date,
         s.site_id, s.name AS site, s.state, u.name AS reporting_unit, l.docket, l.reactor_type, l.reactor_vendor,
         COALESCE(t.title, l.title) AS title, l.operating_mode, l.power_level, l.pdf_url,
         c.summary, c.primary_cause, c.human_error, c.error_type, c.eiis_code, c.system_name, c.component,
         c.failure_mode, c.failure_mechanism, c.fuel_cladding, c.rcs_pressure_boundary, c.containment,
         c.radiological_release, c.release_pathway, c.trip_risk, c.ts_required_shutdown, c.confidence,
         c.prompt_version, c.model,
         a.value AS asp_value, a.finding AS asp_finding, a.significant AS asp_significant
  FROM ler_latest l
  LEFT JOIN units u ON u.docket = l.docket
  LEFT JOIN sites s ON s.site_id = l.site_id
  LEFT JOIN adams_titles t ON t.accession_number = l.accession_number
  LEFT JOIN classification c ON c.accession_number = l.accession_number
  LEFT JOIN asp_precursors a ON a.ler_key = l.ler_key
  WHERE l.docket IS NOT NULL;   -- reactor units and reactor-site ISFSIs; materials-license reports stay out
"""

TAG_FIELDS = [("cause", "who"), ("cause", "operator_fundamentals"), ("barriers", "safety_functions_lost"),
              ("environment", "non_radiological"), ("environment", "outside_agencies_notified")]


def map_unit(name, site_units, all_names):
    """Model-written unit name -> docket: exact name, else unit number within the event's site."""
    if name in all_names:
        return all_names[name]
    m = re.search(r"unit\s*(\d)", name or "", re.I)
    cands = [d for d, u in site_units if m and u == m.group(1)] or ([site_units[0][0]] if len(site_units) == 1 else [])
    return cands[0] if len(cands) == 1 else None


def apply_release_checks(db, version, model):
    """Override the environment fields with the second-pass release check, where it ran."""
    have = db.execute("SELECT count(*) FROM cls.sqlite_master WHERE name='release_checks'").fetchone()[0]
    if not have:
        return 0
    rows = db.execute("SELECT accession_number, result_json FROM cls.release_checks WHERE version=? AND model=? "
                      "AND accession_number IN (SELECT accession_number FROM classification)",
                      (version, model)).fetchall()
    for acc, js in rows:
        r = json.loads(js)
        evidence = " / ".join(x for x in (r["radiological_evidence"], r["non_radiological_evidence"]) if x)
        db.execute("UPDATE classification SET radiological_release=?, environment_evidence=?, release_check=?, "
                   "release_pathway=?, non_radiological_description=? WHERE accession_number=?",
                   (r["radiological_release"], evidence, version, r["radiological_pathway"] or None,
                    r["non_radiological_description"] or None, acc))
        db.execute("DELETE FROM class_tags WHERE accession_number=? AND field='non_radiological'", (acc,))
        for v in r["non_radiological"]:
            db.execute("INSERT INTO class_tags VALUES (?,?,?)", (acc, "non_radiological", v))
    return len(rows)


def build(pv, model, release_version):
    if os.path.exists(OUT):
        os.remove(OUT)
    src = sqlite3.connect("file:%s?mode=ro" % PARSED, uri=True)
    src.execute("VACUUM INTO ?", (OUT,))           # full copy of the parsed db, FTS index included
    src.close()
    db = sqlite3.connect(OUT)
    db.executescript(SCHEMA)
    db.execute("ATTACH ? AS cls", ("file:%s?mode=ro" % CLASSIFIED,))
    db.execute("ATTACH ? AS raw", ("file:%s?mode=ro" % RAW,))
    db.execute("INSERT INTO adams_titles SELECT accession_number, document_title FROM raw.adams_documents")

    all_names = {n: d for d, n in db.execute("SELECT docket, name FROM units")}
    site_units = {}
    for d, site, unit in db.execute("SELECT docket, site_id, unit FROM units"):
        site_units.setdefault(site, []).append((d, unit))
    site_of = dict(db.execute("SELECT accession_number, site_id FROM lers"))

    rows = db.execute("SELECT accession_number, result_json, created_at FROM cls.classifications "
                      "WHERE prompt_version=? AND model=?", (pv, model)).fetchall()
    unmapped = 0
    for acc, js, at in rows:
        r = json.loads(js)
        c, s, b, e, g = r["cause"], r["system"], r["barriers"], r["environment"], r["generation"]
        db.execute("INSERT INTO classification VALUES (%s)" % ",".join("?" * 30), (
            acc, pv, model, at, r["summary"],
            c["primary_cause"], c["human_error"], c["error_type"], c["evidence"],
            s["eiis_code"], s["name"], s["component"], s["failure_mode"], s["failure_mechanism"],
            b["fuel_cladding"], b["rcs_pressure_boundary"], b["containment"], b["evidence"],
            e["radiological_release"], e["evidence"],
            g["trip_risk"], g["ts_required_shutdown"], g["stated_cost_usd"], g["risk_metrics"],
            r["confidence"], r["notes"], js, None, None, None))
        for u in g["units"]:
            d = map_unit(u["unit"], site_units.get(site_of.get(acc), []), all_names)
            unmapped += d is None
            db.execute("INSERT INTO class_generation VALUES (?,?,?,?,?,?,?,?,?)", (
                acc, u["unit"], d, int(u["event_occurred_here"]), u["impact"], u["power_before_pct"],
                u["power_after_pct"], u["duration_hours"], u["duration_evidence"]))
        for grp, f in TAG_FIELDS:
            for v in r[grp][f]:
                db.execute("INSERT INTO class_tags VALUES (?,?,?)", (acc, f, v))
    n_rel = apply_release_checks(db, release_version, model)
    n_asp = 0
    if os.path.exists(ASP):
        db.execute("ATTACH ? AS asp", ("file:%s?mode=ro" % ASP,))
        db.execute("INSERT INTO asp_precursors SELECT * FROM asp.asp_precursors")
        n_asp = db.execute("SELECT count(*) FROM asp_precursors").fetchone()[0]
    db.executescript(POST)
    db.execute("INSERT OR REPLACE INTO meta VALUES ('analysis_classification', ?)", ("%s / %s" % (pv, model),))
    db.execute("INSERT OR REPLACE INTO meta VALUES ('asp_precursors', ?)", (str(n_asp),))
    db.execute("INSERT OR REPLACE INTO meta VALUES ('release_checks', ?)", ("%d / %s" % (n_rel, release_version),))
    db.commit()
    n_gen = db.execute("SELECT COUNT(*) FROM class_generation").fetchone()[0]
    n_ev = db.execute("SELECT COUNT(*), COUNT(summary) FROM events").fetchone()
    print("built %s: %d classifications (%s / %s), %d per-unit generation rows (%d not mapped to a docket), "
          "events view: %d events, %d classified; %d release checks (%s)"
          % (OUT, len(rows), pv, model, n_gen, unmapped, *n_ev, n_rel, release_version))


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--prompt-version", default="p4")
    ap.add_argument("--model", default="claude-haiku-5-5")
    ap.add_argument("--release-version", default="r2", help="classify_release.py version to take environment fields from")
    a = ap.parse_args()
    build(a.prompt_version, a.model, a.release_version)
