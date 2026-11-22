#!/usr/bin/env python3
"""Classify LER events with Claude into lers_classified.db (never touches lers.db or lers_parsed.db).

One request per event (latest revision, from the ler_latest view). Results are keyed by
accession number + prompt version + model, so re-parsing never forces a re-run and a prompt
change only re-runs what it has to.

Usage:
  python3 classify_lers.py --dry-run ML14219A568      # write the exact request for one event, send nothing
  python3 classify_lers.py --pilot 200 --sample-only   # pick + save the pilot sample, send nothing
  python3 classify_lers.py --pilot 200                 # classify the pilot sample (needs ANTHROPIC_API_KEY)
  python3 classify_lers.py --review pilot_review.csv   # spreadsheet of results for checking by hand
  python3 classify_lers.py --cost                      # measured tokens/cost so far + full-run estimate

Needs `pip install anthropic` only for the API calls.
"""
import argparse
import collections
import concurrent.futures as cf
import csv
import datetime as dt
import json
import os
import random
import re
import sqlite3
import sys
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
PARSED_DB = os.path.join(HERE, "lers_parsed.db")
RAW_DB = os.path.join(HERE, "lers.db")
OUT_DB = os.path.join(HERE, "lers_classified.db")
SAMPLE_FILE = os.path.join(HERE, "pilot_sample.txt")

MODEL = "claude-haiku-5-5"   # override with --model
PROMPT_VERSION = "p4"   # p2: per-unit generation, human_error consistency, EIIS candidates are hints;
                        # p3: barrier levels require a physical condition of the barrier itself;
                        # p4: breached = material actually passed the barrier; open-but-not-leaking = degraded
MAX_CHARS = 100_000          # events with more text than this are left out of the pilot (3 of 9,257)
# $ per million tokens (Anthropic price list cached 2026-11-03 -- check before trusting).
# Haiku 5.5 rates apply to prompts <= 100K tokens; longer prompts cost 5x (none in the pilot).
PRICES = {
    "claude-haiku-5-5": {"input": 0.10, "output": 0.50, "cache_read": 0.01, "cache_write": 0.125},
    "claude-sonnet-5-5": {"input": 2.00, "output": 10.00, "cache_read": 0.10, "cache_write": 2.50},
    "claude-opus-5-5": {"input": 4.00, "output": 20.00, "cache_read": 0.20, "cache_write": 5.00},
}
FALLBACK_MODELS = {"claude-opus-5-5", "claude-sonnet-5-5"}   # Haiku 5.5 has no server-side refusal fallback
BATCH_DISCOUNT = 0.5

# ------------------------------------------------------------------ output schema
def enum(*vals):
    return {"type": "string", "enum": list(vals)}


def enum_list(*vals):
    return {"type": "array", "items": enum(*vals)}


def nullable(t):
    return {"anyOf": [{"type": t}, {"type": "null"}]}


def obj(**props):
    return {"type": "object", "properties": props, "required": list(props), "additionalProperties": False}


BARRIER = enum("none", "potential", "challenged", "degraded", "breached")

SCHEMA = obj(
    summary={"type": "string"},
    cause=obj(
        primary_cause=enum("human_performance", "equipment", "design", "procedure", "external", "unknown"),
        human_error=enum("yes", "contributing", "no", "unknown"),
        who=enum_list("licensed_operator", "non_licensed_operator", "maintenance", "i_and_c", "engineering",
                      "contractor", "chemistry", "radiation_protection", "security", "management", "other"),
        error_type=enum("slip_lapse", "rule_based_mistake", "knowledge_based_mistake", "procedure_noncompliance",
                        "not_applicable", "unknown"),
        operator_fundamentals=enum_list("monitoring", "control", "conservatism", "teamwork", "knowledge"),
        evidence={"type": "string"},
    ),
    system=obj(
        eiis_code=nullable("string"),
        name={"type": "string"},
        component={"type": "string"},
        failure_mode=enum("fails_to_start", "fails_to_run", "fails_open", "fails_closed", "spurious_actuation",
                          "leak", "setpoint_drift", "loss_of_power", "degraded_performance", "inoperable_condition",
                          "no_equipment_failure", "other", "unknown"),
        failure_mechanism=enum("wear_aging", "corrosion_erosion", "fatigue_vibration", "foreign_material",
                               "manufacturing_defect", "maintenance_induced", "design_deficiency",
                               "environmental", "electrical_fault", "not_applicable", "other", "unknown"),
    ),
    barriers=obj(
        fuel_cladding=BARRIER,
        rcs_pressure_boundary=BARRIER,
        containment=BARRIER,
        safety_functions_lost=enum_list("shutdown", "heat_removal", "release_control", "accident_mitigation"),
        evidence={"type": "string"},
    ),
    environment=obj(
        radiological_release=enum("none", "within_limits", "above_limits", "unmonitored_path", "unknown"),
        non_radiological=enum_list("oil_chemical_spill", "thermal_discharge", "biota_impact", "other"),
        outside_agencies_notified=enum_list("state", "local", "epa", "coast_guard", "other_federal", "other"),
        evidence={"type": "string"},
    ),
    generation=obj(
        units={"type": "array", "items": obj(
            unit={"type": "string"},
            event_occurred_here={"type": "boolean"},
            impact=enum("none", "reactor_trip", "manual_shutdown", "power_reduction", "delayed_startup",
                        "extended_outage", "already_shutdown", "unknown"),
            power_before_pct=nullable("integer"),
            power_after_pct=nullable("integer"),
            duration_hours=nullable("number"),
            duration_evidence=nullable("string"),
        )},
        trip_risk=enum("yes", "no", "unknown"),
        ts_required_shutdown=enum("yes", "no", "unknown"),
        stated_cost_usd=nullable("number"),
        risk_metrics=nullable("string"),
    ),
    confidence=enum("high", "medium", "low"),
    notes=nullable("string"),
)

SYSTEM_PROMPT = """You classify U.S. nuclear power plant Licensee Event Reports (LERs) for a searchable research database. \
Each request is one event: metadata the database already knows, then the LER text (often OCR from a scanned form, so \
expect broken words and stray characters).

Ground every field in what the LER says. When the LER does not say, use "unknown" or null -- do not infer typical \
values, durations, or costs. Evidence fields are a short quotation (under 40 words) copied from the LER that supports \
the call; use an empty string when the answer is "none"/"unknown" and nothing supports it.

summary: two plain-language sentences: what happened, and why. Name the unit(s).

cause
- primary_cause: the main root or apparent cause the licensee identifies. human_performance = a person's action or \
inaction (including inadequate work practices); procedure = the procedure itself was wrong or missing; design = \
original design or a modification was deficient; equipment = a component failed without a human or design cause \
named; external = weather, grid, wildlife, offsite event.
- human_error: yes if a human error is the primary cause, contributing if it is a named contributor, no otherwise. An inadequate procedure, design, or review that someone prepared is still a human contributor when the LER names the people or process that produced it (e.g. "inadequate procedure revision", "ineffective independent review").
- who, error_type, operator_fundamentals describe that human error. When human_error is "no", who and operator_fundamentals must be empty and error_type must be not_applicable.
- who: every group whose error contributed.
- error_type: slip_lapse = right intent, wrong execution (skill-based); rule_based_mistake = wrong rule or procedure \
step applied; knowledge_based_mistake = wrong diagnosis or understanding; procedure_noncompliance = a step was \
knowingly or carelessly not followed.
- operator_fundamentals: only for errors by operators -- leave empty unless who includes licensed_operator or \
non_licensed_operator. monitoring = monitoring plant \
indications and conditions closely; control = controlling plant evolutions precisely; conservatism = operating with \
a conservative bias; teamwork = working effectively as a team (briefs, peer checks, communication); knowledge = \
understanding of plant design, engineering principles, and sciences.

system
- eiis_code: the two-letter EIIS system code of the system most central to the event -- the system whose \
condition made the event reportable, not just a system mentioned in passing. The codes found in brackets in the text \
(listed in the metadata) are hints only: licensees tag whatever systems they mention, and the central system is \
often untagged. Use your best EIIS code when none of the hints is the central system, else null.
- name: the system name in plain words (e.g. "high pressure coolant injection").
- component: the failed or affected component in a few words (e.g. "turbine stop valve limit switch"), or "none".
- failure_mode / failure_mechanism: for the main equipment problem; no_equipment_failure / not_applicable when the \
event involved no equipment failure (e.g. a missed surveillance).

barriers (the three fission product barriers)
- fuel_cladding, rcs_pressure_boundary, containment -- rate the physical condition of the barrier itself:
  fuel_cladding means the cladding of fuel rods only -- not control, poison, or burnable absorber rods.
  breached = radioactive material or coolant actually passed through the barrier: failed or perforated fuel rods, \
reactor coolant pressure boundary leakage (e.g. a through-wall crack, a leaking weld or seal), containment leakage \
measured beyond its limits, or a release through an open containment path.
  degraded = the barrier's physical integrity was reduced but nothing is shown to have passed through it: fuel \
cladding defects inferred from elevated coolant iodine or off-gas activity without confirmed failed rods, damaged \
fuel assemblies with intact cladding, wall thinning, non-through-wall cracking, a containment isolation valve that \
would not close, or a containment penetration, airlock, or isolation path left open when containment was required \
but with no leakage or release through it.
  challenged = an actual transient or demand stressed the barrier (e.g. a pressure or temperature excursion) but \
it held.
  potential = everything that affected the barrier only on paper or through support equipment: instrument \
channels, trip setpoints, calibration, surveillance or test gaps, analysis or design-basis findings, calculated \
margins, missed Technical Specification actions. Use potential, not degraded, for these even if the LER calls the \
system inoperable.
  none = the barrier was not involved.
- safety_functions_lost: safety functions the LER says were (or could have been) lost -- these mirror \
10 CFR 50.73(a)(2)(v)(A)-(D).

environment
- radiological_release: release of radioactive material offsite or to the environment (unmonitored_path = e.g. \
tritium in groundwater, leak to a storm drain).
- non_radiological: non-radiological environmental effects described in the LER.
- outside_agencies_notified: agencies other than the NRC that the LER says were notified.

generation
- units: one entry per unit in "Units involved" in the metadata, plus any other unit the text says was affected. \
unit = the unit name as given in the metadata. event_occurred_here = true for the unit(s) where the event or \
condition actually happened, which is not necessarily the reporting unit (multi-unit sites often report under \
Unit 1). A unit that was not affected gets impact "none" with its power level if stated.
- impact: the event's effect on that unit's electrical output. already_shutdown = the unit was already shut down or \
refueling when the event happened.
- power_before_pct / power_after_pct: that unit's reactor power in percent; after = the lowest power reached because \
of the event (0 after a reactor trip or a shutdown to subcritical).
- duration_hours: hours that unit was off line or reduced because of this event, only if the LER states it or gives \
times/dates that determine it; duration_evidence quotes them.
- trip_risk: yes if the event did not trip the unit but the LER describes a real chance it could have (e.g. one \
more failure away from a scram, a transient caught in time, a shutdown action statement that nearly expired).
- ts_required_shutdown: yes if Technical Specifications required a shutdown (whether or not it was completed).
- stated_cost_usd: only a dollar figure the LER states.
- risk_metrics: quoted risk numbers if the LER gives them (e.g. "delta CDF 3E-7/yr, ICCDP 1E-8"), else null.

confidence: how well the text supports the classification overall (low if the text is badly garbled or very short).
notes: anything a reviewer should know (e.g. the LER covers several unrelated events), else null."""

# ------------------------------------------------------------------ inputs
EIIS_RE = re.compile(r"\[\s*([A-Z]{2})\s*\]")


def connect_ro(path):
    return sqlite3.connect("file:%s?mode=ro" % path, uri=True)


def event_text(pdb, rdb, acc):
    row = pdb.execute("""SELECT l.accession_number, l.ler_number, l.ler_key, l.docket, l.title, l.event_date,
                                l.report_date, l.operating_mode, l.power_level, l.reactor_type, l.reactor_vendor,
                                l.abstract, l.narrative, l.content_chars, u.name, s.name, s.state
                         FROM lers l LEFT JOIN units u ON u.docket = l.docket LEFT JOIN sites s ON s.site_id = l.site_id
                         WHERE l.accession_number = ?""", (acc,)).fetchone()
    if not row:
        sys.exit("unknown accession %s" % acc)
    (acc, ler, key, docket, title, ev, rp, mode, power, rtype, vendor, abstract, narrative, nchars,
     unit, site, state) = row
    units = pdb.execute("""SELECT x.docket, COALESCE(u.name, x.docket), x.role, x.affected FROM ler_units x
                           LEFT JOIN units u ON u.docket = x.docket WHERE x.accession_number = ?
                           ORDER BY x.role DESC, x.docket""", (acc,)).fetchall()
    cfr = [r[0] for r in pdb.execute("SELECT cfr_section FROM ler_cfr WHERE accession_number=? ORDER BY 1", (acc,))]
    body_src = "narrative"
    body = narrative
    if not body:
        body = rdb.execute("SELECT content FROM adams_documents WHERE accession_number=?", (acc,)).fetchone()[0]
        body_src = "full document text (no narrative section was found)"
    adams_title = rdb.execute("SELECT document_title FROM adams_documents WHERE accession_number=?", (acc,)).fetchone()[0]
    eiis = collections.Counter(EIIS_RE.findall((abstract or "") + "\n" + (body or "")))
    meta = [
        ("LER number", ler), ("Site", "%s (%s)" % (site, state) if site else None), ("Reporting unit", unit or docket),
        ("Units involved", "; ".join("%s [%s%s]" % (n, role, "" if aff else ", named but not affected")
                                     for _, n, role, aff in units) or None),
        ("Reactor type / NSSS vendor", " / ".join(x for x in (rtype, vendor) if x) or None),
        ("Event date", ev), ("Report date", rp), ("Operating mode (form)", mode),
        ("Power level % (form)", power),
        ("10 CFR sections cited", ", ".join(cfr) or None),
        ("EIIS system codes in text (count)", ", ".join("%s (%d)" % kv for kv in eiis.most_common(12)) or None),
    ]
    lines = ["<metadata>"] + ["%s: %s" % (k, v if v not in (None, "") else "not available") for k, v in meta]
    lines += ["</metadata>", "", "<title source=\"ADAMS index\">%s</title>" % (adams_title or ""),
              "<title source=\"LER form, may run into neighboring form fields\">%s</title>" % (title or ""), "",
              "<abstract>\n%s\n</abstract>" % (abstract or "not available"), "",
              "<text source=\"%s\">\n%s\n</text>" % (body_src, body or "")]
    return "\n".join(lines), key, nchars


def build_request(pdb, rdb, acc, effort):
    user, key, nchars = event_text(pdb, rdb, acc)
    req = {
        "model": MODEL,
        "max_tokens": 16000,
        "system": [{"type": "text", "text": SYSTEM_PROMPT, "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": user}],
        "output_config": {"effort": effort, "format": {"type": "json_schema", "schema": SCHEMA}},
    }
    return req, key


# ------------------------------------------------------------------ sample
def pick_sample(pdb, n, seed=20261114):
    rows = pdb.execute("SELECT accession_number, document_date, reactor_type FROM ler_latest "
                       "WHERE content_chars BETWEEN 500 AND ? AND reactor_type IN ('PWR', 'BWR')", (MAX_CHARS,)).fetchall()
    strata = collections.defaultdict(list)
    for acc, d, rt in rows:
        dec = "pre-1990" if (d or "0") < "1990" else d[:3] + "0s"
        strata[(dec, rt)].append(acc)
    rnd = random.Random(seed)
    for v in strata.values():
        rnd.shuffle(v)
    keys = sorted(strata)
    out = []
    while len(out) < n and any(strata.values()):     # round-robin across strata so small ones are represented
        for k in keys:
            if strata[k] and len(out) < n:
                out.append(strata[k].pop())
    return out


def load_or_make_sample(pdb, n):
    if os.path.exists(SAMPLE_FILE):
        accs = [l.split()[0] for l in open(SAMPLE_FILE) if l.strip() and not l.startswith("#")]
        if len(accs) >= n:
            return accs[:n]
    accs = pick_sample(pdb, n)
    with open(SAMPLE_FILE, "w") as f:
        f.write("# pilot sample, stratified by decade x reactor type, seed 20261114\n")
        f.write("\n".join(accs) + "\n")
    return accs


# ------------------------------------------------------------------ output db
OUT_SCHEMA = """
CREATE TABLE IF NOT EXISTS classifications (
    accession_number TEXT, prompt_version TEXT, model TEXT, effort TEXT, ler_key TEXT,
    result_json TEXT,
    summary TEXT, primary_cause TEXT, human_error TEXT, eiis_code TEXT, failure_mode TEXT,
    generation_impact TEXT, confidence TEXT,
    input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER,
    stop_reason TEXT, request_id TEXT, served_model TEXT, created_at TEXT,
    PRIMARY KEY (accession_number, prompt_version, model)
);
CREATE TABLE IF NOT EXISTS errors (accession_number TEXT, prompt_version TEXT, model TEXT, error TEXT, created_at TEXT);
CREATE TABLE IF NOT EXISTS batches (
    batch_id TEXT PRIMARY KEY, prompt_version TEXT, model TEXT, effort TEXT, n_requests INTEGER,
    submitted_at TEXT, collected_at TEXT
);
"""


def out_db():
    db = sqlite3.connect(OUT_DB, check_same_thread=False, timeout=30)
    db.executescript(OUT_SCHEMA)
    cols = {r[1] for r in db.execute("PRAGMA table_info(classifications)")}
    if "via" not in cols:   # added with batch support; older rows were all synchronous calls
        db.execute("ALTER TABLE classifications ADD COLUMN via TEXT DEFAULT 'sync'")
        db.commit()
    return db


def store(db, acc, key, effort, msg, request_id, via):
    """Save one finished message; returns a status string. Non-end_turn stops go to the errors table."""
    now = dt.datetime.now().isoformat(timespec="seconds")
    if msg.stop_reason != "end_turn":
        db.execute("INSERT INTO errors VALUES (?,?,?,?,?)",
                   (acc, PROMPT_VERSION, MODEL, "stop_reason=%s %s" % (msg.stop_reason, getattr(msg, "stop_details", None)), now))
        db.commit()
        return "stop_reason %s" % msg.stop_reason
    text = next(b.text for b in msg.content if b.type == "text")
    res = json.loads(text)
    u = msg.usage
    db.execute("""INSERT OR REPLACE INTO classifications (accession_number, prompt_version, model, effort, ler_key,
                  result_json, summary, primary_cause, human_error, eiis_code, failure_mode, generation_impact, confidence,
                  input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, stop_reason, request_id,
                  served_model, created_at, via) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""", (
        acc, PROMPT_VERSION, MODEL, effort, key, text,
        res["summary"], res["cause"]["primary_cause"], res["cause"]["human_error"], res["system"]["eiis_code"],
        res["system"]["failure_mode"], "; ".join("%s: %s" % (x["unit"], x["impact"]) for x in res["generation"]["units"]),
        res["confidence"], u.input_tokens, u.output_tokens, u.cache_read_input_tokens or 0,
        u.cache_creation_input_tokens or 0, msg.stop_reason, request_id, msg.model, now, via))
    db.commit()
    return "ok"


def pending(db, accs):
    done = {r[0] for r in db.execute("SELECT accession_number FROM classifications WHERE prompt_version=? AND model=?",
                                     (PROMPT_VERSION, MODEL))}
    return [a for a in accs if a not in done]


BATCH_CHUNK = 2000   # requests per batch; ~25 KB each keeps a batch well under the 256 MB limit


def all_events(pdb):
    """Full run: latest revision of every power-reactor (PWR/BWR) event with usable text."""
    return [r[0] for r in pdb.execute("SELECT accession_number FROM ler_latest WHERE reactor_type IN ('PWR', 'BWR') "
                                      "AND content_chars >= 500 ORDER BY accession_number")]


def submit_batch(accs, effort):
    """Send all not-yet-classified events as one Message Batch (50% off; results within 24 h, usually < 1 h)."""
    import anthropic
    from anthropic.types.message_create_params import MessageCreateParamsNonStreaming
    from anthropic.types.messages.batch_create_params import Request
    client = anthropic.Anthropic()
    db = out_db()
    todo = pending(db, accs)
    print("%d requested, %d already done, %d to send as a batch" % (len(accs), len(accs) - len(todo), len(todo)))
    if not todo:
        return None
    p, r = connect_ro(PARSED_DB), connect_ro(RAW_DB)
    ids = []
    for i in range(0, len(todo), BATCH_CHUNK):
        chunk = todo[i:i + BATCH_CHUNK]
        reqs = [Request(custom_id=acc, params=MessageCreateParamsNonStreaming(**build_request(p, r, acc, effort)[0]))
                for acc in chunk]
        batch = client.messages.batches.create(requests=reqs)
        db.execute("INSERT INTO batches VALUES (?,?,?,?,?,?,NULL)", (batch.id, PROMPT_VERSION, MODEL, effort, len(reqs),
                                                                     dt.datetime.now().isoformat(timespec="seconds")))
        db.commit()
        print("submitted batch %s (%d requests)" % (batch.id, len(reqs)), flush=True)
        ids.append(batch.id)
    return ids


def collect_batches(wait):
    """Collect results of every submitted batch not yet collected; with wait, poll until each one ends."""
    import time
    import anthropic
    client = anthropic.Anthropic()
    db = out_db()
    p = connect_ro(PARSED_DB)
    open_batches = db.execute("SELECT batch_id, prompt_version, model, effort FROM batches WHERE collected_at IS NULL").fetchall()
    if not open_batches:
        print("no uncollected batches")
    for bid, pv, model, effort in open_batches:
        globals()["PROMPT_VERSION"], globals()["MODEL"] = pv, model   # store under the batch's own version/model
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
        for res in client.messages.batches.results(bid):       # results arrive in any order: key by custom_id
            acc = res.custom_id
            key = (p.execute("SELECT ler_key FROM lers WHERE accession_number=?", (acc,)).fetchone() or [None])[0]
            if res.result.type == "succeeded":
                status = store(db, acc, key, effort, res.result.message, "%s/%s" % (bid, acc), "batch")
            else:
                err = res.result.error.type if res.result.type == "errored" else res.result.type
                db.execute("INSERT INTO errors VALUES (?,?,?,?,?)", (acc, pv, model, "batch %s: %s" % (bid, err),
                                                                     dt.datetime.now().isoformat(timespec="seconds")))
                db.commit()
                status = "batch %s" % err
            tally[status] += 1
        db.execute("UPDATE batches SET collected_at=? WHERE batch_id=?", (dt.datetime.now().isoformat(timespec="seconds"), bid))
        db.commit()
        print("collected %s: %s" % (bid, dict(tally)))
        cost()


def classify(accs, effort, workers):
    import anthropic                      # only needed here
    client = anthropic.Anthropic()
    db = out_db()
    todo = pending(db, accs)
    print("%d requested, %d already done, %d to send" % (len(accs), len(accs) - len(todo), len(todo)))
    lock = threading.Lock()
    # each worker opens its own read connections
    local = threading.local()

    def conns():
        if not hasattr(local, "p"):
            local.p, local.r = connect_ro(PARSED_DB), connect_ro(RAW_DB)
        return local.p, local.r

    def one(acc):
        p, r = conns()
        req, key = build_request(p, r, acc, effort)
        now = dt.datetime.now().isoformat(timespec="seconds")
        try:
            if MODEL in FALLBACK_MODELS:
                # server-side fallback: if a safety classifier declines, the API re-runs on a fallback model
                resp = client.beta.messages.create(**req, betas=["server-side-fallback-2026-07-01"], fallbacks="default")
            else:
                resp = client.messages.create(**req)   # a refusal is logged to the errors table
        except anthropic.APIStatusError as e:
            with lock:
                db.execute("INSERT INTO errors VALUES (?,?,?,?,?)",
                           (acc, PROMPT_VERSION, MODEL, "%s %s" % (e.status_code, e.message), now))
                db.commit()
            return acc, "error %s" % e.status_code
        except anthropic.APIConnectionError as e:
            with lock:
                db.execute("INSERT INTO errors VALUES (?,?,?,?,?)", (acc, PROMPT_VERSION, MODEL, "connection: %s" % e, now))
                db.commit()
            return acc, "connection error"
        with lock:
            return acc, store(db, acc, key, effort, resp, resp._request_id, "sync")

    n = 0
    with cf.ThreadPoolExecutor(workers) as ex:
        for acc, status in ex.map(one, todo):
            n += 1
            print("  [%d/%d] %s %s" % (n, len(todo), acc, status), flush=True)
    cost()


# ------------------------------------------------------------------ reporting
def cost():
    db = out_db()
    PRICE = PRICES[MODEL]
    n = inp = out = cr = cw = 0
    usd = 0.0
    for via, k, i_, o_, cr_, cw_ in db.execute("""SELECT via, COUNT(*), SUM(input_tokens), SUM(output_tokens),
            SUM(cache_read_tokens), SUM(cache_write_tokens) FROM classifications WHERE prompt_version=? AND model=?
            GROUP BY via""", (PROMPT_VERSION, MODEL)):
        mult = BATCH_DISCOUNT if via == "batch" else 1.0
        usd += mult * (i_ * PRICE["input"] + o_ * PRICE["output"] + cr_ * PRICE["cache_read"] + cw_ * PRICE["cache_write"]) / 1e6
        n, inp, out, cr, cw = n + k, inp + i_, out + o_, cr + cr_, cw + cw_
    if not n:
        print("no results yet for %s / %s" % (MODEL, PROMPT_VERSION))
        return
    total = connect_ro(PARSED_DB).execute("SELECT COUNT(*) FROM ler_latest").fetchone()[0]
    print("model %s, prompt %s" % (MODEL, PROMPT_VERSION))
    print("%d events classified: %d input, %d output, %d cache-read, %d cache-write tokens" % (n, inp, out, cr, cw))
    print("cost so far $%.2f  (avg $%.4f/event, %d output tokens/event; batch results priced at 50%%)"
          % (usd, usd / n, out / n))
    print("full run, %d events: ~$%.0f at this mix of rates" % (total, usd / n * total))


def review(path):
    db = out_db()
    pdb = connect_ro(PARSED_DB)
    rdb = connect_ro(RAW_DB)
    rows = db.execute("SELECT accession_number, result_json FROM classifications WHERE prompt_version=? AND model=? "
                      "ORDER BY accession_number", (PROMPT_VERSION, MODEL)).fetchall()
    cols = None
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        for acc, js in rows:
            res = json.loads(js)
            flat = {"accession": acc}
            info = list(pdb.execute("SELECT l.ler_number, u.name, l.event_date, l.title, l.pdf_url FROM lers l "
                                    "LEFT JOIN units u ON u.docket=l.docket WHERE accession_number=?", (acc,)).fetchone())
            info[3] = rdb.execute("SELECT document_title FROM adams_documents WHERE accession_number=?",
                                  (acc,)).fetchone()[0] or info[3]       # ADAMS title; the form title often overruns
            flat.update(zip(["ler_number", "unit", "event_date", "title", "pdf"], info))
            for k, v in res.items():
                if isinstance(v, dict):
                    for k2, v2 in v.items():
                        if isinstance(v2, list) and v2 and isinstance(v2[0], dict):   # per-unit generation
                            v2 = ["%s%s: %s (%s%% -> %s%%%s)" % (
                                u["unit"], " [event]" if u["event_occurred_here"] else "", u["impact"],
                                u["power_before_pct"], u["power_after_pct"],
                                ", %sh" % u["duration_hours"] if u["duration_hours"] is not None else "") for u in v2]
                        flat["%s.%s" % (k, k2)] = "; ".join(v2) if isinstance(v2, list) else v2
                else:
                    flat[k] = v
            flat["reviewer_ok"] = ""
            flat["reviewer_notes"] = ""
            if cols is None:
                cols = list(flat)
                w.writerow(cols)
            w.writerow([flat.get(c) for c in cols])
    print("wrote %d rows to %s" % (len(rows), path))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dry-run", metavar="ACCESSION", help="write the exact request for one event and send nothing")
    ap.add_argument("--pilot", type=int, metavar="N", help="classify N sampled events")
    ap.add_argument("--all", action="store_true", help="classify every PWR/BWR event via the Batches API")
    ap.add_argument("--sample-only", action="store_true", help="with --pilot: just pick and save the sample")
    ap.add_argument("--batch", action="store_true", help="with --pilot: submit via the Batches API (50%% off) and wait")
    ap.add_argument("--collect", action="store_true", help="collect results of submitted batches (add --wait to poll)")
    ap.add_argument("--wait", action="store_true")
    ap.add_argument("--review", metavar="CSV")
    ap.add_argument("--cost", action="store_true")
    ap.add_argument("--effort", default="medium", choices=["low", "medium", "high", "xhigh", "max"])
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--model", default=MODEL, choices=sorted(PRICES))
    ap.add_argument("--version", default=PROMPT_VERSION,
                    help="prompt version to report on with --review/--cost (classification always uses the current one)")
    a = ap.parse_args()
    globals()["MODEL"] = a.model
    if a.review or a.cost:
        globals()["PROMPT_VERSION"] = a.version
    if a.dry_run:
        req, _ = build_request(connect_ro(PARSED_DB), connect_ro(RAW_DB), a.dry_run, a.effort)
        out = os.path.join(HERE, "request_%s.json" % a.dry_run)
        with open(out, "w") as f:
            json.dump(req, f, indent=2)
        print(req["messages"][0]["content"][:4000])
        print("\n... full request (system prompt, text, schema) written to %s (%d chars of user text)"
              % (out, len(req["messages"][0]["content"])))
    elif a.pilot:
        accs = load_or_make_sample(connect_ro(PARSED_DB), a.pilot)
        print("pilot sample: %d events (%s)" % (len(accs), SAMPLE_FILE))
        if a.sample_only:
            pass
        elif a.batch:
            if submit_batch(accs, a.effort):
                collect_batches(wait=True)
        else:
            classify(accs, a.effort, a.workers)
    elif a.all:
        if submit_batch(all_events(connect_ro(PARSED_DB)), a.effort):
            collect_batches(wait=True)
    elif a.collect:
        collect_batches(wait=a.wait)
    elif a.review:
        review(a.review)
    elif a.cost:
        cost()
    else:
        ap.print_help()


if __name__ == "__main__":
    main()
