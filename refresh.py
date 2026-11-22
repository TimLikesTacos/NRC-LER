#!/usr/bin/env python3
"""Bring the LER data up to date: pull new ADAMS LERs, re-parse, classify new events, rebuild, check.

  python3 refresh.py                 # full refresh
  python3 refresh.py --dry-run       # print the steps, run nothing
  python3 refresh.py --skip-pull     # re-parse / classify / rebuild from the lers.db you have
  python3 refresh.py --skip-classify # new events show up unclassified (and check_data will fail)
  python3 refresh.py --since 2026-01-01

Steps
  1. back up lers.db to lers.db.pre-refresh.bak (one rolling copy; lers.db only ever gains rows)
  2. aps.py ingest, type "Licensee Event Report (LER)", since <newest document date - 60 days>
     (only --since: since+until together seems to drop the date filter; stored documents are skipped)
  3. parse_lers.py            lers.db -> lers_parsed.db
  4. classify_lers.py --all   only events not yet classified, via the Batches API (waits for results)
  4a. classify_release.py --all   second-pass environmental release check of new candidate events (batch, waits)
  4b. pull_asp.py, parse_asp.py   NRC precursor (ASP) results -> asp.db -> asp_parsed.db
  5. build_analysis.py        -> lers_analysis.db (previous copy kept as lers_analysis.prev.db)
  6. check_data.py            if it fails, the previous lers_analysis.db is put back

Keys come from .env (NRC_APS_KEY, ANTHROPIC_API_KEY) and are passed to the steps in the environment.
The MCP server keeps its open database until restarted: reconnect it (/mcp) after a refresh.
"""
import argparse
import datetime as dt
import os
import shutil
import sqlite3
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RAW = os.path.join(HERE, "lers.db")
ANALYSIS = os.path.join(HERE, "lers_analysis.db")
PREV = os.path.join(HERE, "lers_analysis.prev.db")
LER_TYPE = "Licensee Event Report (LER)"
OVERLAP_DAYS = 60


def load_env(path=os.path.join(HERE, ".env")):
    """KEY=VALUE lines from .env, without overriding the real environment. Values are never printed."""
    env = dict(os.environ)
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                line = line.strip()
                if line and not line.startswith("#") and "=" in line:
                    k, v = line.split("=", 1)
                    env.setdefault(k.strip().removeprefix("export "), v.strip().strip("'\""))
    pylib = os.path.join(HERE, ".pylib")
    if os.path.isdir(pylib):
        env["PYTHONPATH"] = os.pathsep.join(p for p in (pylib, env.get("PYTHONPATH")) if p)
    return env


def raw_stats():
    db = sqlite3.connect("file:%s?mode=ro" % RAW, uri=True)
    return db.execute("SELECT count(*), max(document_date), max(date_added) FROM adams_documents").fetchone()


def default_since():
    newest = raw_stats()[1]
    return (dt.date.fromisoformat(newest[:10]) - dt.timedelta(days=OVERLAP_DAYS)).isoformat()


def run(step, cmd, env, dry):
    print("\n== %s: %s" % (step, " ".join(cmd)), flush=True)
    if dry:
        return
    t = time.time()
    r = subprocess.run(cmd, cwd=HERE, env=env)
    print("== %s: exit %d in %.0fs" % (step, r.returncode, time.time() - t), flush=True)
    if r.returncode:
        raise SystemExit("refresh stopped: %s failed" % step)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--since", help="ADAMS document date to pull from (default: newest stored - %d days)" % OVERLAP_DAYS)
    ap.add_argument("--skip-pull", action="store_true")
    ap.add_argument("--skip-classify", action="store_true")
    ap.add_argument("--skip-asp", action="store_true", help="don't pull new NRC precursor documents")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    env, py, dry = load_env(), sys.executable, a.dry_run
    before = raw_stats()
    print("lers.db: %d documents, newest document %s, last added %s" % before)

    if not a.skip_pull:
        for k in ("NRC_APS_KEY",) + (() if a.skip_classify else ("ANTHROPIC_API_KEY",)):
            if not env.get(k):
                sys.exit("%s is not set (put it in .env)" % k)
        if not dry:
            shutil.copy2(RAW, RAW + ".pre-refresh.bak")
        run("pull", [py, "aps.py", "ingest", "--db", RAW, "--type", LER_TYPE, "--since", a.since or default_since()],
            env, dry)
        after = raw_stats()
        print("new documents: %d" % (after[0] - before[0]))

    run("parse", [py, "parse_lers.py"], env, dry)
    if not a.skip_classify:
        run("classify", [py, "classify_lers.py", "--all"], env, dry)
        run("release check", [py, "classify_release.py", "--all"], env, dry)
    if not (a.skip_pull or a.skip_asp):
        run("pull ASP", [py, "pull_asp.py"], env, dry)
    if os.path.exists(os.path.join(HERE, "asp.db")) or dry:
        run("parse ASP", [py, "parse_asp.py"], env, dry)
    if os.path.exists(ANALYSIS) and not dry:
        shutil.copy2(ANALYSIS, PREV)
    run("build", [py, "build_analysis.py"], env, dry)
    try:
        run("check", [py, "check_data.py"], env, dry)
    except SystemExit:
        if os.path.exists(PREV):
            shutil.copy2(PREV, ANALYSIS)
            print("restored the previous lers_analysis.db; the new parse is still in lers_parsed.db")
        raise
    print("\nrefresh done. Reconnect the MCP server (/mcp) so it opens the new lers_analysis.db.")


if __name__ == "__main__":
    main()
