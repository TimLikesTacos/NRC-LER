#!/usr/bin/env python3
"""Second-pass check of environmental releases, into lers_classified.db table release_checks.

The main classification (classify_lers.py, prompt p4) tags radiological_release loosely: routine
containment venting during a leak, leaks that stayed inside containment, lost sources, and reports
that say "no release" all come back as releases, and non_radiological picks up intake fouling and
oil leaks inside buildings. This pass asks one narrow question -- did the event put material into
the environment? -- for the candidate events only, and build_analysis.py lets its answer override
the p4 environment fields. Every other p4 field is left alone.

Candidates: events p4 tagged with any release (including "unknown") or a non-radiological effect,
plus events whose text matches release words (tritium, storm drain, spill, NPDES, ...).

Usage:
  python3 classify_release.py --candidates             # count candidates, send nothing
  python3 classify_release.py --dry-run ML14211A526    # write the exact request for one event
  python3 classify_release.py --pilot                  # the hand-picked cases in PILOT, synchronous
  python3 classify_release.py --all                    # every candidate not yet checked, Batches API, waits
  python3 classify_release.py --collect --wait         # collect batches left uncollected
  python3 classify_release.py --review release_review.csv
  python3 classify_release.py --cost
"""
import argparse
import collections
import concurrent.futures as cf
import csv
import datetime as dt
import json
import sqlite3
import sys
import threading
import time

from classify_lers import (BATCH_CHUNK, BATCH_DISCOUNT, PARSED_DB, PRICES, RAW_DB, OUT_DB, connect_ro, enum,
                           enum_list, event_text, obj)

VERSION = "r2"   # r1: pilot; r2: pathway/description written before the category, lost sources and
                 # secondary containment spelled out as none, oil inside a building as no release
MODEL = "claude-haiku-5-5"
EFFORT = "medium"

# Pathway and description come before the category so the model says where the material went before it
# picks one (r1 tagged an oil leak "on the floor" as a spill).
SCHEMA = obj(
    radiological_pathway={"type": "string"},
    radiological_evidence={"type": "string"},
    radiological_release=enum("none", "within_limits", "above_limits", "unmonitored_path", "unknown"),
    non_radiological_description={"type": "string"},
    non_radiological_evidence={"type": "string"},
    non_radiological=enum_list("oil_chemical_spill", "thermal_discharge", "permit_exceedance", "other"),
    notes={"anyOf": [{"type": "string"}, {"type": "null"}]},
)

SYSTEM_PROMPT = """You check one thing in U.S. nuclear power plant Licensee Event Reports (LERs): did the event \
release material into the environment? Each request is one LER: metadata, then the report text (often OCR from a \
scanned form, so expect broken words).

The environment means outside the plant's buildings and systems: the atmosphere, surface water (lake, river, \
ocean, canal, discharge to a receiving water), groundwater, soil or the yard, storm drains, or anywhere offsite. \
Material that stays inside containment, the reactor building, auxiliary building, turbine building, secondary \
containment (including the standby gas treatment system), tanks, sumps, floors, or plant systems has not reached \
the environment. Leakage from primary into secondary containment is not a release.

For each kind of release, first write where the material actually went (pathway / description), then choose the \
category that fits that destination. If the material never got outside, the category is none / an empty list.

Ground every call in what the LER says. Evidence fields are a short quotation (under 40 words) copied from the LER; \
use an empty string when the answer is none and nothing supports it.

radiological_release -- radioactive material the event caused to reach the environment:
- above_limits: the LER says a release exceeded a regulatory, Technical Specification, ODCM, or 10 CFR 20 limit.
- unmonitored_path: radioactive material reached the environment by a path that is not a monitored effluent \
release point: spills or leaks to the yard, soil, or storm drains; tritium or other radionuclides in groundwater; \
contaminated water reaching service water, circulating water, or another discharge that is not monitored for it; \
leakage out of a building through a door, drain, or penetration. Use this even when the amount was within limits.
- within_limits: an unplanned or abnormal release, caused by the event, through a monitored effluent path (plant \
vent, stack, monitored liquid discharge), within limits. Examples: a rupture disk or relief valve lifting and the \
release going out the monitored ventilation; a steam generator tube leak releasing activity through the condenser \
air ejector or steam to atmosphere; a waste gas or liquid waste release made by mistake.
- none: nothing reached the environment because of the event. In particular these are NOT releases:
  - a degraded or breached fuel cladding, reactor coolant pressure boundary, or containment barrier, or reactor \
coolant leakage, that stayed inside the plant. Do not infer a release from a barrier problem; only count one if the \
LER says radioactive material actually left the plant.
  - routine or planned effluent that the event did not cause: a scheduled containment purge, a normal containment \
pressure relief, a planned waste gas decay tank release -- even if the LER mentions it and its dose. Controlled, \
procedure-directed venting or purging of containment through the normal monitored path is routine effluent even \
when the event made it necessary; it is a release only if the LER treats it as unplanned, abnormal, or reportable.
  - releases that could have happened, analyzed or hypothetical doses, and "no release occurred" statements.
  - effluent monitor or sampling problems (an inoperable radiation monitor, a missed or non-representative sample) \
with no actual release caused by the event.
  - lost, missing, or misplaced radioactive sources, detectors, or special nuclear material, even if they may \
have gone out with trash or to a landfill: that is a material-control problem, not a release -- use none, not \
unknown; shipping or transport problems; contamination of people or equipment inside the plant.
- unknown: the LER says radioactive effluent may have reached the environment because of the event but gives no \
way to tell. Do not use unknown for lost sources or for paths that only could have released during an accident.
When more than one applies, use the most severe: above_limits, then unmonitored_path, then within_limits.

radiological_pathway: where the material went, in a few words (e.g. "rupture disk, auxiliary building, monitored \
plant vent"; "condensate spill to yard and storm drain"), or "" for none.

non_radiological -- non-radioactive material or heat the event put into the environment:
- oil_chemical_spill: oil, fuel, chemicals, sewage, or other hazardous material that reached soil, groundwater, \
storm drains, or surface water, or was released to the atmosphere (e.g. a refrigerant or toxic gas release). An oil \
or chemical leak that stayed inside a building, on a floor, in a room, a drip pan, a dike, or a containment curb is \
not a release, however large; neither is one where the LER does not say it got outside.
- thermal_discharge: cooling water discharged above a temperature, temperature-change, or thermal permit limit.
- permit_exceedance: a discharge that exceeded an NPDES or other environmental permit limit, other than thermal.
- other: another actual release to the environment (describe it).
Leave the list empty when nothing was released. Fish or wildlife drawn into the intake, intake fouling by jellyfish, \
clams, or debris, and wildlife takes are not releases. Environmental reporting to a state or agency is not by itself \
a release.

non_radiological_description: what was released and where, in a few words, or "".
notes: anything a reviewer should know, else null."""

# Hand-picked cases for the pilot: (accession, what a person reading the LER expects).
PILOT = [
    ("ML14211A526", "release: rupture disk, tritium + noble gas out the monitored vent (Millstone 2014-006)"),
    ("ML19143A375", "none: steam leak inside drywell; containment venting is routine (Brunswick 2019-002)"),
    ("ML13295A018", "none: RCS packing leak inside containment; pressure relief routine (Salem 2013-001)"),
    ("ML12090A153", "release: SG tube leak, monitored release (San Onofre 2012-001)"),
    ("ML072120588", "unmonitored: tritiated condensate to switchyard/yard (Salem 2 2007-002)"),
    ("ML070890308", "unmonitored: tritiated condensate to yard (Palo Verde 2006-005)"),
    ("ML020430199", "none: RCS into service water, 'not an offsite release' (Crystal River 2001-003)"),
    ("ML003704179", "none: 'did not result in an abnormal release' (Farley 2000-003)"),
    ("ML111720731", "none: 'did not constitute an unmonitored release' (Kewaunee 1978-015)"),
    ("ML17252B545", "none: leakage into secondary containment (Dresden 1979-017)"),
    ("ML043220341", "none: detectors to landfill = lost source (Clinton 2004-006)"),
    ("ML022120412", "none: lost source (Catawba 2002-005)"),
    ("ML062640018", "none: lost SNM (Hatch 2005-003)"),
    ("ML003718811", "none: non-representative off-gas samples (Nine Mile Point 2000-009)"),
    ("ML050180262", "none: unmonitored path open, but only 'could have' released (D.C. Cook 2004-004)"),
    ("ML043560178", "release: turbine building pipe break (Hope Creek 2004-010)"),
    ("ML19095A699", "unmonitored: tank overflow to storm drains (Surry 1975-027)"),
    ("ML18018B238", "unmonitored: contaminated liquid under pump house door (Dresden 1977-012)"),
    ("ML16030A542", "release: air ejector gaseous release (Oconee 1977-016)"),
    ("ML051370438", "check: motor oil leak (Columbia 2005-002)"),
    ("ML14191A633", "check: 2 gal lube oil leak (Perry 2014-002)"),
    ("ML11301A071", "none: jellyfish at intake (St. Lucie 2011-001)"),
    ("ML033180115", "none: alewives at intake (D.C. Cook 2003-003)"),
    ("ML18353B234", "thermal: discharge 38F above lake (Palisades 1975-007)"),
    ("ML18348A250", "thermal: above 5F limit, NPDES not exceeded (Palisades 1977-015)"),
]

CANDIDATE_TEXT = [
    'tritium', '"storm drain" OR "storm drains" OR "storm sewer"', 'groundwater OR "ground water"',
    '"unmonitored release" OR "unmonitored path" OR "unplanned release"',
    '"released to the environment" OR "release to the environment" OR "offsite release" OR "radioactive release"',
    'spill OR spilled OR spills', '"oil sheen" OR "oil spill"',
    'NPDES OR "discharge permit" OR "environmental protection plan"',
    'hypochlorite OR "fish kill"', '"national response center"',
]

OUT_SCHEMA = """
CREATE TABLE IF NOT EXISTS release_checks (
    accession_number TEXT, version TEXT, model TEXT, result_json TEXT,
    radiological_release TEXT, non_radiological TEXT,
    input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER,
    via TEXT, request_id TEXT, created_at TEXT,
    PRIMARY KEY (accession_number, version, model)
);
CREATE TABLE IF NOT EXISTS release_batches (
    batch_id TEXT PRIMARY KEY, version TEXT, model TEXT, n_requests INTEGER, submitted_at TEXT, collected_at TEXT
);
"""


def now():
    return dt.datetime.now().isoformat(timespec="seconds")


def out_db():
    db = sqlite3.connect(OUT_DB, check_same_thread=False, timeout=30)
    db.executescript(OUT_SCHEMA)
    return db


def candidates():
    """Accessions to check: p4-tagged releases/effects plus text matches, PWR/BWR events with p4 results."""
    p = connect_ro(PARSED_DB)
    p.execute("ATTACH ? AS cls", ("file:%s?mode=ro" % OUT_DB,))
    power = {r[0] for r in p.execute("SELECT accession_number FROM ler_latest WHERE reactor_type IN ('PWR','BWR') "
                                     "AND content_chars >= 500")}
    tagged = set()
    for acc, js in p.execute("SELECT accession_number, result_json FROM cls.classifications "
                             "WHERE prompt_version='p4' AND model=?", (MODEL,)):
        e = json.loads(js)["environment"]
        if e["radiological_release"] != "none" or e["non_radiological"]:
            tagged.add(acc)
    text = set()
    for q in CANDIDATE_TEXT:
        text |= {r[0] for r in p.execute("SELECT accession_number FROM lers_fts WHERE lers_fts MATCH ?", (q,))}
    return sorted((tagged | text) & power), len(tagged & power), len((text - tagged) & power)


def build_request(p, r, acc):
    user, _, _ = event_text(p, r, acc)
    return {
        "model": MODEL,
        "max_tokens": 8000,
        "system": [{"type": "text", "text": SYSTEM_PROMPT, "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": user}],
        "output_config": {"effort": EFFORT, "format": {"type": "json_schema", "schema": SCHEMA}},
    }


def store(db, acc, msg, request_id, via):
    if msg.stop_reason != "end_turn":
        db.execute("INSERT INTO errors VALUES (?,?,?,?,?)", (acc, VERSION, MODEL, "stop_reason=%s" % msg.stop_reason, now()))
        db.commit()
        return "stop_reason %s" % msg.stop_reason
    text = next(b.text for b in msg.content if b.type == "text")
    res = json.loads(text)
    u = msg.usage
    db.execute("INSERT OR REPLACE INTO release_checks VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)", (
        acc, VERSION, MODEL, text, res["radiological_release"], ",".join(res["non_radiological"]),
        u.input_tokens, u.output_tokens, u.cache_read_input_tokens or 0, u.cache_creation_input_tokens or 0,
        via, request_id, now()))
    db.commit()
    return "ok"


def pending(db, accs):
    done = {r[0] for r in db.execute("SELECT accession_number FROM release_checks WHERE version=? AND model=?",
                                     (VERSION, MODEL))}
    return [a for a in accs if a not in done]


def run_sync(accs, workers=4):
    import anthropic
    client = anthropic.Anthropic()
    db = out_db()
    todo = pending(db, accs)
    print("%d requested, %d to send" % (len(accs), len(todo)))
    lock, local = threading.Lock(), threading.local()

    def one(acc):
        if not hasattr(local, "p"):
            local.p, local.r = connect_ro(PARSED_DB), connect_ro(RAW_DB)
        try:
            resp = client.messages.create(**build_request(local.p, local.r, acc))
        except (anthropic.APIStatusError, anthropic.APIConnectionError) as e:
            with lock:
                db.execute("INSERT INTO errors VALUES (?,?,?,?,?)", (acc, VERSION, MODEL, str(e)[:500], now()))
                db.commit()
            return acc, "error"
        with lock:
            return acc, store(db, acc, resp, resp._request_id, "sync")

    with cf.ThreadPoolExecutor(workers) as ex:
        for i, (acc, status) in enumerate(ex.map(one, todo), 1):
            print("  [%d/%d] %s %s" % (i, len(todo), acc, status), flush=True)
    cost()


def submit_batches(accs):
    import anthropic
    from anthropic.types.message_create_params import MessageCreateParamsNonStreaming
    from anthropic.types.messages.batch_create_params import Request
    client = anthropic.Anthropic()
    db = out_db()
    todo = pending(db, accs)
    print("%d candidates, %d already checked, %d to send as a batch" % (len(accs), len(accs) - len(todo), len(todo)))
    p, r = connect_ro(PARSED_DB), connect_ro(RAW_DB)
    for i in range(0, len(todo), BATCH_CHUNK):
        reqs = [Request(custom_id=a, params=MessageCreateParamsNonStreaming(**build_request(p, r, a)))
                for a in todo[i:i + BATCH_CHUNK]]
        b = client.messages.batches.create(requests=reqs)
        db.execute("INSERT INTO release_batches VALUES (?,?,?,?,?,NULL)", (b.id, VERSION, MODEL, len(reqs), now()))
        db.commit()
        print("submitted batch %s (%d requests)" % (b.id, len(reqs)), flush=True)


def collect(wait):
    import anthropic
    client = anthropic.Anthropic()
    db = out_db()
    for bid, version, model in db.execute("SELECT batch_id, version, model FROM release_batches "
                                          "WHERE collected_at IS NULL").fetchall():
        if (version, model) != (VERSION, MODEL):
            print("skipping batch %s (%s / %s, not the current %s / %s)" % (bid, version, model, VERSION, MODEL))
            continue
        while True:
            b = client.messages.batches.retrieve(bid)
            c = b.request_counts
            print("batch %s: %s (processing %d, succeeded %d, errored %d)"
                  % (bid, b.processing_status, c.processing, c.succeeded, c.errored), flush=True)
            if b.processing_status == "ended" or not wait:
                break
            time.sleep(60)
        if b.processing_status != "ended":
            continue
        tally = collections.Counter()
        for res in client.messages.batches.results(bid):
            if res.result.type == "succeeded":
                tally[store(db, res.custom_id, res.result.message, "%s/%s" % (bid, res.custom_id), "batch")] += 1
            else:
                db.execute("INSERT INTO errors VALUES (?,?,?,?,?)",
                           (res.custom_id, VERSION, MODEL, "batch %s: %s" % (bid, res.result.type), now()))
                tally[res.result.type] += 1
        db.execute("UPDATE release_batches SET collected_at=? WHERE batch_id=?", (now(), bid))
        db.commit()
        print("collected %s: %s" % (bid, dict(tally)))
    cost()


def cost():
    db = out_db()
    pr = PRICES[MODEL]
    n, usd = 0, 0.0
    for via, k, i, o, cr, cw in db.execute(
            "SELECT via, COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cache_read_tokens), SUM(cache_write_tokens) "
            "FROM release_checks WHERE version=? AND model=? GROUP BY via", (VERSION, MODEL)):
        usd += (BATCH_DISCOUNT if via == "batch" else 1) * (i * pr["input"] + o * pr["output"] + cr * pr["cache_read"]
                                                            + cw * pr["cache_write"]) / 1e6
        n += k
    print("%s / %s: %d events checked, $%.2f so far" % (VERSION, MODEL, n, usd))


def review(path):
    """CSV: p4's call next to this pass's, with the pilot expectation where there is one."""
    db = out_db()
    p = connect_ro(PARSED_DB)
    expect = dict(PILOT)
    p4 = {a: json.loads(js)["environment"] for a, js in db.execute(
        "SELECT accession_number, result_json FROM classifications WHERE prompt_version='p4' AND model=?", (MODEL,))}
    rows = db.execute("SELECT accession_number, result_json FROM release_checks WHERE version=? AND model=? "
                      "ORDER BY accession_number", (VERSION, MODEL)).fetchall()
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["accession", "ler_number", "unit", "event_date", "pdf", "expected", "p4_radiological",
                    "radiological", "pathway", "radiological_evidence", "p4_non_radiological", "non_radiological",
                    "non_radiological_description", "non_radiological_evidence", "notes", "reviewer_ok", "reviewer_notes"])
        for acc, js in rows:
            r = json.loads(js)
            ler, unit, ev, pdf = p.execute("SELECT l.ler_number, u.name, l.event_date, l.pdf_url FROM lers l LEFT JOIN "
                                           "units u ON u.docket = l.docket WHERE accession_number=?", (acc,)).fetchone()
            old = p4.get(acc, {})
            w.writerow([acc, ler, unit, ev, pdf, expect.get(acc, ""), old.get("radiological_release"),
                        r["radiological_release"], r["radiological_pathway"], r["radiological_evidence"],
                        ";".join(old.get("non_radiological", [])), ";".join(r["non_radiological"]),
                        r["non_radiological_description"], r["non_radiological_evidence"], r["notes"], "", ""])
    print("wrote %d rows to %s" % (len(rows), path))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--candidates", action="store_true")
    ap.add_argument("--dry-run", metavar="ACCESSION")
    ap.add_argument("--pilot", action="store_true")
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--collect", action="store_true")
    ap.add_argument("--wait", action="store_true")
    ap.add_argument("--review", metavar="CSV")
    ap.add_argument("--cost", action="store_true")
    a = ap.parse_args()
    if a.candidates:
        accs, n_tagged, n_text = candidates()
        print("%d candidates: %d tagged by p4, %d more from text search" % (len(accs), n_tagged, n_text))
    elif a.dry_run:
        req = build_request(connect_ro(PARSED_DB), connect_ro(RAW_DB), a.dry_run)
        path = "release_request_%s.json" % a.dry_run
        json.dump(req, open(path, "w"), indent=1)
        print("wrote %s (%d chars of user text)" % (path, len(req["messages"][0]["content"])))
    elif a.pilot:
        run_sync([acc for acc, _ in PILOT])
    elif a.all:
        submit_batches(candidates()[0])
        collect(wait=True)
    elif a.collect:
        collect(a.wait)
    elif a.review:
        review(a.review)
    elif a.cost:
        cost()
    else:
        ap.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
