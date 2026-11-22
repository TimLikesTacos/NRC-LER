# NRC Licensee Event Reports

On the NRC website and in ADAMS, it's hard to find **Licensee Event Reports (LERs)** unless you
search for a keyword that appears in that particular LER. LERs are written by different licensees, their format has changed
over time, and many older ones exist only as scanned documents with OCR text. That makes it hard to
aggregate LERs or find similar causes and events with existing tools.

This project uses AI to classify events and put LERs into a common structure, so LER data can be
analyzed quickly. Example uses:

- Trend analysis
- Common-cause analysis
- Finding similar events

This is useful for licensing and training departments at nuclear power plants.

This project is a searchable database of US NRC LERs, the reports commercial nuclear power plants
must file after reactor trips, safety system actuations, Technical Specification
violations and similar events, plus an **MCP server** (`ler-mcp`) that lets an AI assistant
(Claude Desktop, Claude Code, or any MCP client) search and summarize them.

- About 9,200 events from the NRC ADAMS public library, well covered for 1999-2026 and 1976-1978
  (other years are sparse: see [Coverage](#coverage)).
- Each event has the parsed report fields (plant, unit, dates, LER number, reporting criteria,
  abstract, full narrative) plus an **AI classification** made by Claude Haiku from the report
  text: cause, human performance (who erred, INPO operator fundamentals), system and failure
  mode, fission product barrier condition, environmental release (radiological and
  non-radiological), safety functions lost,
  generation impact per unit.
- NRC **Accident Sequence Precursor (ASP)** risk estimates (CCDP / delta-CDP) are joined in for
  the events the NRC rated, so "most risk-significant" questions use the NRC's own numbers.

## Quick start: use the MCP server

You need the prebuilt database `lers_analysis.db` (about 430 MB, 116 MB gzipped) and the
`ler-mcp` binary.

### 1. Get the database

Download `lers_analysis.db.gz` from the [latest release](../../releases/latest), then:

```sh
gunzip lers_analysis.db.gz
```

(Or build it yourself: see [Rebuilding the data](#rebuilding-the-data).)

### 2. Get the server

Prebuilt binaries for Linux, macOS and Windows are attached to each [release](../../releases/latest).
To build from source instead, you need Rust (edition 2024, so Rust 1.85 or newer). SQLite is bundled, so no system library is needed.

```sh
cd ler-mcp
cargo build --release
# binary: ler-mcp/target/release/ler-mcp
```

### 3. Connect a client

The server takes the database path as its argument (or the `LER_DB` environment variable).
There is no default, because MCP clients don't guarantee a working directory, so use an absolute path.

**Claude Code**

```sh
claude mcp add ler -- /abs/path/ler-mcp/target/release/ler-mcp /abs/path/lers_analysis.db
```

**Claude Desktop** (`claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "ler": {
      "command": "/abs/path/ler-mcp/target/release/ler-mcp",
      "args": ["/abs/path/lers_analysis.db"]
    }
  }
}
```

**HTTP** (one long-running server for several clients):

```sh
ler-mcp /abs/path/lers_analysis.db --http 127.0.0.1:8090
# endpoint: http://127.0.0.1:8090/mcp
```

The HTTP transport accepts only loopback `Host` headers, which guards against DNS rebinding.
To expose it beyond localhost, put it behind a reverse proxy rather than binding a public address.

Logging goes to stderr. Set `RUST_LOG=debug` (or `ler_mcp=debug`) for more detail.

### Tools

| Tool | What it does |
|---|---|
| `search_events` | Filter and sort events by plant, date, system, full text, cause, human error, who erred, operator fundamentals, barrier condition, environmental release (radiological, or non-radiological such as oil spills and thermal discharges), safety functions lost, reporting criteria, reactor type, precursor status. Sorts: newest, oldest, relevance, severity, risk. |
| `get_event` | One LER by number (`327-2014-002`, `05000327/2014-002-01`) or ADAMS accession number (`ML14219A568`): full record, classification, PDF link. |
| `get_event_text` | Sections of the report text (abstract, narrative, cause, corrective actions, previous events, ...). |
| `summarize_events` | Counts grouped by a field (year, plant, cause, system, ...) under the same filters as search. |
| `list_precursors` | NRC ASP precursors, ranked by CCDP / delta-CDP. |

Example questions:

- "What's the most recent LER involving auxiliary feedwater at Sequoyah?"
- "Which events came closest to core damage since 2010?"
- "How many human-performance events were there each year, by who erred?"
- "Show recent events where operators showed weak conservatism."
- "Show significant events that were caused by not following the procedure."

### Sample output

What the server returns for those questions (data as of November 2026, trimmed for length). Each
example shows the tool call an assistant makes, then the key fields of the result. The assistant
writes its own answer from these. Cause, who erred and fundamentals come from the AI
classification; the precursor values are the NRC's.

<details>
<summary><b>"What's the most recent LER involving auxiliary feedwater at Sequoyah?"</b></summary>

```json
search_events {"plant": "Sequoyah", "system": ["BA", "auxiliary feedwater"], "limit": 1}
```

| LER | Event date | Summary |
|---|---|---|
| Sequoyah 1, 2013-002-00 (ML13120A265) | 2013-02-08 | Two fuses blew in a shared instrument loop, leaving the auxiliary feedwater flow indicators in the auxiliary control room without power longer than Technical Specification 3.3.3.5 allowed. Cause: equipment. Reported as a safety function failure. |

</details>

<details>
<summary><b>"Which events came closest to core damage since 2010?"</b></summary>

```json
list_precursors {"date_from": "2010", "limit": 3}
```

121 precursors since 2010, riskiest first, by the NRC's CCDP (conditional core damage probability):

| Plant, LER | Event date | CCDP | Event |
|---|---|---|---|
| Duane Arnold, 2020-001 | 2020-08-10 | 8×10⁻⁴ | Loss of offsite power from high winds, unit trip, Unusual Event |
| Waterford 3, 2021-001 | 2021-08-29 | 5×10⁻⁴ | Loss of offsite power from Hurricane Ida |
| Watts Bar 1, 2015-006 | 2015-10-19 | 1×10⁻⁴ | Both source range trip channels inoperable during startup |

None since 2010 reached the NRC's "significant precursor" level (1×10⁻³).

</details>

<details>
<summary><b>"How many human-performance events were there each year, by who erred?"</b></summary>

```json
summarize_events {"group_by": "year", "then_by": "who", "human_error": ["yes", "contributing"], "date_from": "2022"}
```

| Year | Events with human error | Top groups (an event can name several) |
|---|---|---|
| 2022 | 63 | maintenance 26, management 16, engineering 15, licensed operator 8 |
| 2023 | 58 | maintenance 21, engineering 15, other 12, management 9 |
| 2024 | 71 | management 25, maintenance 24, engineering 21, licensed operator 16 |
| 2025 | 68 | maintenance 25, engineering 16, management 15, licensed operator 13 |

</details>

<details>
<summary><b>"Show recent events where operators showed weak conservatism."</b></summary>

```json
search_events {"operator_fundamentals": ["conservatism"], "limit": 3}
```

127 events, newest first:

| LER | Event date | Summary | Fundamentals |
|---|---|---|---|
| Oconee 2, 2026-001-00 | 2025-11-14 | An auxiliary operator opened a potential transformer drawer to check clearance tags; the false low-voltage signal opened the main feeder bus breakers for 22 seconds and started both Keowee hydro units. Supervision didn't validate the risk of the task ("success bias"). | conservatism, knowledge |
| Callaway 1, 2025-001-00 | 2025-05-04 | The refueling water storage tank dropped below its Technical Specification minimum while used as makeup water, because the crew set no level band. | monitoring, conservatism, teamwork |
| Arkansas Nuclear One 1, 2024-002-00 | 2024-12-10 | Containment penetration isolation valves found open at 67% power. | conservatism |

</details>

<details>
<summary><b>"Show significant events that were caused by not following the procedure."</b></summary>

```json
search_events {"cause": ["procedure", "human_performance"],
               "text": "\"procedure not followed\" OR \"failure to follow\" OR \"did not follow\"",
               "sort": "severity", "limit": 2}
```

107 events, most severe first:

| LER | Event date | Summary | Reported as |
|---|---|---|---|
| Indian Point 2, 2013-002-00 | 2013-04-16 | Maintenance opened a diesel generator day tank fill valve without tagout protection while the normal fuel oil header was tagged out, which could have left the other two diesel generators short of fuel oil. The tagging procedure wasn't followed. | unanalyzed condition, safety function failure, common cause |
| Monticello, 2013-006-01 | 2013-08-21 | A fuel oil cross-connect left open meant both emergency diesel generators relied on manual operator actions that were never part of the licensing basis. | safety function failure, common cause |

</details>

## Coverage

Only the main ADAMS library was pulled. Most older LERs are in its legacy library, so:

- **Well covered:** 1976-1978 and 1999-2025 (100+ reports a year).
- **Sparse:** 1971-1975 and 1979-1998 (about 357 reports in all). An event from these years that
  isn't found is most likely missing from this database, not nonexistent.
- **Current year:** LERs are due 60 days after discovery, so the most recent couple of months
  are always incomplete.
- Only the latest revision of each LER is classified. ISFSI (spent fuel storage) reports are
  included but not classified, and materials-license reports (hospitals, gauges) are excluded.

The server reports the exact "data as of" date in its instructions to the client.

## Caveats

- **The AI classification is a first pass.** It was made by Claude Haiku from the report text and
  is useful for screening and counting, but it makes mistakes (barrier ratings especially). For
  anything that matters, read the report itself: every result links its PDF.
- Environmental releases come from a second, narrower pass (`classify_release.py`): a release
  means material the event put into the environment. A degraded or breached barrier, a leak that
  stayed inside the plant, routine effluent, or a lost source doesn't count.
- The "severity" sort is a rough screening score that mixes the AI barrier ratings with the
  reporting criteria the licensee cited. It is not a risk measure. Use the "risk" sort (NRC ASP
  values) for risk.
- LER numbers, titles and dates are parsed from OCR'd and form-generated PDFs. ADAMS's own index
  fields are hand-keyed and sometimes wrong, and the parser picks between them by source
  reliability. A small number of mismatches remain.
- This project is not affiliated with or endorsed by the NRC.

## Rebuilding the data

You only need this to refresh the data or change the parser/classifier. The pipeline is plain
Python 3.12 (standard library only, except the `anthropic` SDK for classification).

### Keys

Put these in `.env` (never commit it):

```sh
NRC_APS_KEY=...        # free: https://adams-api-developer.nrc.gov/ -> "ADAMS Public Search API" -> Subscribe
ANTHROPIC_API_KEY=...  # for classify_lers.py only
```

```sh
pip install anthropic   # or: pip install --target .pylib anthropic, then run with PYTHONPATH=.pylib
```

### Pipeline

| Step | Script | Output |
|---|---|---|
| Pull LERs from ADAMS (raw text + metadata) | `aps.py ingest` | `lers.db` |
| Parse fields, units, LER numbers, CFR criteria, FTS index | `parse_lers.py` | `lers_parsed.db` |
| Classify events with Claude Haiku (Batches API) | `classify_lers.py --all` | `lers_classified.db` |
| Recheck environmental releases on candidate events (narrower second pass) | `classify_release.py --all` | `lers_classified.db` (`release_checks`) |
| Pull and parse NRC ASP precursor reports | `pull_asp.py`, `parse_asp.py` | `asp.db`, `asp_parsed.db` |
| Join everything into the query database | `build_analysis.py` | `lers_analysis.db` |
| Sanity checks against the last baseline | `check_data.py` | `check_data_last.json` |

`refresh.py` runs all of it incrementally: it backs up `lers.db`, pulls documents dated from
60 days before the newest stored one, re-parses, classifies only new events, rebuilds, and restores
the previous `lers_analysis.db` if the checks fail.

```sh
set -a; . ./.env; set +a
python3 refresh.py --dry-run   # show the steps
python3 refresh.py             # full refresh
python3 refresh.py --skip-pull # rebuild from the lers.db you have
```

A first pull from scratch:

```sh
python3 aps.py ingest --db lers.db --type "Licensee Event Report (LER)"
```

The ingest is resumable (stored accessions are skipped). Don't pass `--since` and `--until`
together, because the ADAMS API appears to drop the date filter when both are given.

Classifying everything from scratch costs real money (about 9,000 Haiku calls at Batches
pricing). Run `python3 classify_lers.py --cost` to estimate it first.

Useful debugging:

```sh
python3 parse_lers.py --show ML14219A568   # how one document parsed
python3 parse_lers.py --coverage           # field fill rates by decade
python3 check_data.py --show METRIC        # rows behind one check metric
```

New plant dockets that show up in later pulls must be added by hand to `plants_ref.py`.

### MCP server tests

```sh
cd ler-mcp
cargo test                                                                     # unit tests, in-memory fixtures
LER_DB=/abs/path/lers_analysis.db cargo test evals -- --ignored --nocapture   # question evals, evals/questions.json
```

## Releasing

Pushing a `v*` tag runs `.github/workflows/release.yml`, which tests and builds `ler-mcp` for
Linux (x86_64, arm64), macOS (arm64, x86_64) and Windows, and attaches the binaries and
`SHA256SUMS` to a GitHub Release. The database isn't built in CI, so upload it yourself:

```sh
git tag v0.1.0 && git push origin v0.1.0
gzip -9 -k lers_analysis.db && gh release upload v0.1.0 lers_analysis.db.gz
```

## Data sources and license

- LER text and metadata: [NRC ADAMS](https://adams.nrc.gov/wba/) via the ADAMS Public Search API.
- Precursor estimates: NRC ASP program reports (SECY papers and annual reports), also from ADAMS.

Code: MIT (see `LICENSE`). NRC documents are US government works and in the public domain. The classifications are
machine-generated and provided as is, without warranty.
