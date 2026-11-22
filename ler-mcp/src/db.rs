//! Database access. Query functions take a plain `&Connection` so tests can run them against an
//! in-memory database; `Db` wraps a read-only connection pool and runs them off the async runtime.

use std::path::Path;
use std::time::Duration;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::types::{FromSql, Value};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params_from_iter};
use thiserror::Error;

use crate::ids::{EventId, LerRef};
use crate::plants::PlantIndex;
use crate::report::{
    Barriers, Breakdown, Candidate, Candidates, Classification, Coverage, DataInfo, EventReport,
    EventSummary, EventText, GroupCount, Precursor, PrecursorEntry, PrecursorList, SearchResults,
    TextSection, UnitImpact,
};
use crate::vocab::{
    BarrierState, Cause, Criterion, HumanError, NonRadiologicalRelease, OperatorFundamental,
    RadiologicalRelease, ReactorType, SafetyFunction, Who,
};

#[derive(Debug, Error)]
pub enum DbError {
    #[error("database file not found: {0}")]
    Missing(String),
    #[error("database pool: {0}")]
    Pool(#[from] r2d2::Error),
    #[error("database query: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("database task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Most candidates listed for an ambiguous id. A year-sequence without a docket matches every
/// plant that filed that number (about 70 for a typical 2010s number).
pub const MAX_CANDIDATES: usize = 20;

/// Result of resolving an `EventId` and loading what was asked for (`EventReport`, `EventText`).
#[derive(Debug, PartialEq)]
pub enum Lookup<T> {
    Found(Box<T>),
    Ambiguous(Candidates),
    NotFound,
}

// ---------------------------------------------------------------- data info

/// Counts and dates describing how current the database is. `meta` (written by
/// build_analysis.py) is optional so an older or hand-made database still opens.
pub fn data_info(conn: &Connection) -> rusqlite::Result<DataInfo> {
    let (events, classified, newest_report): (i64, i64, Option<String>) = conn.query_row(
        "SELECT count(*), count(summary), max(CASE WHEN report_date <= date('now') THEN report_date END) \
         FROM events",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let newest_added = conn.query_row("SELECT max(date_added) FROM lers", [], |r| r.get(0))?;
    let has_meta: bool = conn.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE name = 'meta'",
        [],
        |r| r.get(0),
    )?;
    let meta = |k: &str| -> rusqlite::Result<Option<String>> {
        if !has_meta {
            return Ok(None);
        }
        conn.query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
            .optional()
    };
    // Year 1950+ and not in the future: bad scans give dates like 1900-01-01 or 2095.
    let by_year = conn
        .prepare(
            "SELECT CAST(substr(COALESCE(event_date, report_date), 1, 4) AS INTEGER) AS y, count(*) \
             FROM events WHERE y BETWEEN 1950 AND CAST(strftime('%Y', 'now') AS INTEGER) \
             GROUP BY y ORDER BY y",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let current_year = newest_report
        .as_deref()
        .and_then(|d: &str| d.get(..4)?.parse().ok());
    Ok(DataInfo {
        events,
        classified,
        newest_report,
        newest_added,
        built_at: meta("built_at")?,
        parser_version: meta("parser_version")?,
        coverage: Coverage {
            by_year,
            current_year,
        },
    })
}

// ---------------------------------------------------------------- pool

/// A small pool of read-only connections, so concurrent tool calls don't share one connection
/// (rusqlite's `Connection` is `Send` but not `Sync`) and don't reopen the file every call.
#[derive(Clone)]
pub struct Db {
    pool: Pool<SqliteConnectionManager>,
    plants: std::sync::Arc<PlantIndex>,
    info: std::sync::Arc<DataInfo>,
}

impl Db {
    /// Opens the pool and checks that the file is an analysis database (has the `events` view).
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if !path.is_file() {
            return Err(DbError::Missing(path.display().to_string()));
        }
        let manager = SqliteConnectionManager::file(path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX);
        let pool = Pool::builder()
            .max_size(4)
            .connection_timeout(Duration::from_secs(5))
            .build(manager)?;
        let conn = pool.get()?;
        conn.query_row("SELECT count(*) FROM events LIMIT 1", [], |_| Ok(()))?;
        let plants = std::sync::Arc::new(PlantIndex::load(&conn)?);
        let info = std::sync::Arc::new(data_info(&conn)?);
        drop(conn);
        Ok(Self { pool, plants, info })
    }

    pub fn plants(&self) -> &PlantIndex {
        &self.plants
    }

    pub fn info(&self) -> &DataInfo {
        &self.info
    }

    /// Runs `find_event` on a blocking thread: SQLite calls block, and blocking a tokio worker
    /// would stall every other request on that thread.
    pub async fn find_event(&self, id: EventId) -> Result<Lookup<EventReport>, DbError> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || -> Result<Lookup<EventReport>, DbError> {
            let conn = pool.get()?;
            Ok(find_event(&conn, &id)?)
        })
        .await?
    }

    /// Runs `find_text` on a blocking thread.
    pub async fn find_text(
        &self,
        id: EventId,
        req: TextRequest,
    ) -> Result<Lookup<EventText>, DbError> {
        self.blocking(move |conn| find_text(conn, &id, &req)).await
    }

    /// Runs `search_events` on a blocking thread (see `find_event`).
    pub async fn search_events(&self, search: Search) -> Result<SearchResults, DbError> {
        self.blocking(move |conn| search_events(conn, &search))
            .await
    }

    /// Runs `summarize_events` on a blocking thread.
    pub async fn summarize_events(&self, s: Summarize) -> Result<Breakdown, DbError> {
        self.blocking(move |conn| summarize_events(conn, &s)).await
    }

    /// Runs `list_precursors` on a blocking thread.
    pub async fn list_precursors(&self, q: PrecursorQuery) -> Result<PrecursorList, DbError> {
        self.blocking(move |conn| list_precursors(conn, &q)).await
    }

    /// Runs a query on a pooled connection off the async runtime.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, DbError> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || -> Result<T, DbError> { Ok(f(&*pool.get()?)?) }).await?
    }
}

// ---------------------------------------------------------------- filters

/// Filters shared by `search_events` and `summarize_events`, already validated. Every filter that
/// is set must match; list filters match if the event has any of the listed values.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    /// Any of these: a two-letter EIIS system code, compared exactly, or else a substring of the
    /// classified system name. (Codes aren't substring-matched: "BA" would hit "balance of plant".)
    pub systems: Vec<String>,
    /// FTS5 query over title, abstract and narrative.
    pub text: Option<String>,
    /// What was typed for the plant. Resolved to `dockets` by `PlantIndex` before querying;
    /// without that, matched as a substring of the site or unit name.
    pub plant: Option<String>,
    /// Reporting dockets the plant stands for.
    pub dockets: Option<Vec<String>>,
    /// Date prefixes ("2020", "2020-06", "2020-06-15"), inclusive at both ends.
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub reactor_types: Vec<ReactorType>,
    pub causes: Vec<Cause>,
    pub human_error: Vec<HumanError>,
    pub who: Vec<Who>,
    pub operator_fundamentals: Vec<OperatorFundamental>,
    pub fuel_cladding: Vec<BarrierState>,
    pub rcs_pressure_boundary: Vec<BarrierState>,
    pub containment: Vec<BarrierState>,
    /// Matches if any one of the three barriers is in one of these states.
    pub any_barrier: Vec<BarrierState>,
    pub radiological_release: Vec<RadiologicalRelease>,
    /// Multi-valued (class_tags): matches if any one was released.
    pub non_radiological_release: Vec<NonRadiologicalRelease>,
    /// Only events that released anything to the environment: a radiological release
    /// (within limits, above limits, unmonitored path) or any non-radiological one.
    pub environmental_release: bool,
    /// Multi-valued (class_tags): matches if any one was lost.
    pub safety_functions_lost: Vec<SafetyFunction>,
    /// Matches if the report cites any of these.
    pub reported_under: Vec<Criterion>,
    /// Only events the NRC rated as accident sequence precursors.
    pub precursor: bool,
    /// Only precursors with a CCDP/delta-CDP of at least this.
    pub min_ccdp: Option<f64>,
}

/// SQL true when event `e` cites the criterion: one of its sections or a subsection of one
/// (`Criterion::matches_section` in SQL).
fn cites(criterion: Criterion) -> String {
    let any: Vec<String> = criterion
        .cfrs()
        .iter()
        .map(|cfr| {
            format!("lower(rc.cfr_section) = '{cfr}' OR lower(rc.cfr_section) LIKE '{cfr}(%'")
        })
        .collect();
    format!(
        "EXISTS (SELECT 1 FROM ler_cfr rc WHERE rc.accession_number = e.accession_number \
         AND ({}))",
        any.join(" OR ")
    )
}

/// The three barrier columns of `events`.
const BARRIERS: [&str; 3] = ["fuel_cladding", "rcs_pressure_boundary", "containment"];

/// 0 (none) to 4 (breached) for a barrier column.
fn barrier_level(column: &str) -> String {
    format!(
        "(CASE {column} WHEN 'breached' THEN 4 WHEN 'degraded' THEN 3 WHEN 'challenged' THEN 2 \
         WHEN 'potential' THEN 1 ELSE 0 END)"
    )
}

/// Rough severity score of an event, NULL when unclassified. From the AI classification: fuel
/// cladding counts double (closest to core damage), then RCS pressure boundary and containment
/// (0-4 each), a radiological release (within limits 2, unmonitored path 4, above limits 6),
/// 2 per safety function lost. From the licensee: the points of each `Criterion` cited.
pub fn severity_sql() -> String {
    let criteria: String = Criterion::ALL
        .iter()
        .filter(|c| c.points() > 0)
        .map(|c| {
            format!(
                " + (CASE WHEN {} THEN {} ELSE 0 END)",
                cites(*c),
                c.points()
            )
        })
        .collect();
    format!(
        "(CASE WHEN e.primary_cause IS NULL THEN NULL ELSE \
         2 * {} + {} + {} \
         + (CASE e.radiological_release WHEN 'above_limits' THEN 6 WHEN 'unmonitored_path' THEN 4 \
            WHEN 'within_limits' THEN 2 ELSE 0 END) \
         + 2 * (SELECT count(*) FROM class_tags sf WHERE sf.accession_number = e.accession_number \
                AND sf.field = 'safety_functions_lost'){criteria} END)",
        barrier_level("e.fuel_cladding"),
        barrier_level("e.rcs_pressure_boundary"),
        barrier_level("e.containment"),
    )
}

/// Names of the criteria an event cites, in `Criterion` order.
fn criteria_cited(db: &Connection, acc: &str) -> rusqlite::Result<Vec<Criterion>> {
    let cited: Vec<String> = db
        .prepare_cached("SELECT lower(cfr_section) FROM ler_cfr WHERE accession_number = ?1")?
        .query_map([acc], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Criterion::ALL
        .iter()
        .filter(|crit| cited.iter().any(|c| crit.matches_section(c)))
        .copied()
        .collect())
}

/// Radiological release values that mean something reached the environment, as SQL.
const RELEASED: &str = "'within_limits', 'above_limits', 'unmonitored_path'";

/// Event date, falling back to the report date when the form's event date wasn't parsed.
const SEARCH_DATE: &str = "COALESCE(e.event_date, e.report_date)";

/// `FROM events e [JOIN lers_fts f] [WHERE ...]` for `filters`, pushing bind values onto
/// `params`. `text` is passed separately so a failed FTS query can be retried quoted.
fn from_where(filters: &Filters, text: Option<&str>, params: &mut Vec<Value>) -> String {
    let mut sql = String::from(" FROM events e");
    let mut wheres: Vec<String> = vec![];
    if let Some(t) = text {
        sql.push_str(" JOIN lers_fts f ON f.accession_number = e.accession_number");
        wheres.push("lers_fts MATCH ?".into());
        params.push(Value::Text(t.into()));
    }
    if !filters.systems.is_empty() {
        let any: Vec<&str> = filters
            .systems
            .iter()
            .map(|s| {
                if is_eiis_code(s) {
                    params.push(Value::Text(s.clone()));
                    "e.eiis_code = ? COLLATE NOCASE"
                } else {
                    params.push(Value::Text(contains_pattern(s)));
                    "e.system_name LIKE ? ESCAPE '\\'"
                }
            })
            .collect();
        wheres.push(format!("({})", any.join(" OR ")));
    }
    if let Some(d) = &filters.dockets {
        wheres.push(format!("e.docket IN ({})", placeholders(d, params)));
    } else if let Some(p) = &filters.plant {
        let pat = contains_pattern(p);
        wheres.push("(e.site LIKE ? ESCAPE '\\' OR e.reporting_unit LIKE ? ESCAPE '\\')".into());
        params.push(Value::Text(pat.clone()));
        params.push(Value::Text(pat));
    }
    if let Some(d) = &filters.date_from {
        wheres.push(format!("{SEARCH_DATE} >= ?"));
        params.push(Value::Text(d.clone()));
    }
    if let Some(d) = &filters.date_to {
        // Compare only as many characters as given, so "2020" includes all of 2020.
        wheres.push(format!("substr({SEARCH_DATE}, 1, {}) <= ?", d.len()));
        params.push(Value::Text(d.clone()));
    }
    for (column, values) in [
        ("e.reactor_type", names(&filters.reactor_types)),
        ("e.primary_cause", names(&filters.causes)),
        ("e.human_error", names(&filters.human_error)),
        ("e.fuel_cladding", names(&filters.fuel_cladding)),
        (
            "e.rcs_pressure_boundary",
            names(&filters.rcs_pressure_boundary),
        ),
        ("e.containment", names(&filters.containment)),
        (
            "e.radiological_release",
            names(&filters.radiological_release),
        ),
    ] {
        if !values.is_empty() {
            wheres.push(format!("{column} IN ({})", placeholders(&values, params)));
        }
    }
    for (field, values) in [
        ("who", names(&filters.who)),
        (
            "operator_fundamentals",
            names(&filters.operator_fundamentals),
        ),
        (
            "safety_functions_lost",
            names(&filters.safety_functions_lost),
        ),
        (
            "non_radiological",
            names(&filters.non_radiological_release),
        ),
    ] {
        if !values.is_empty() {
            wheres.push(format!(
                "EXISTS (SELECT 1 FROM class_tags t WHERE t.accession_number = e.accession_number
                         AND t.field = '{field}' AND t.value IN ({}))",
                placeholders(&values, params)
            ));
        }
    }
    if filters.environmental_release {
        wheres.push(format!(
            "(e.radiological_release IN ({RELEASED}) OR EXISTS (SELECT 1 FROM class_tags nr \
             WHERE nr.accession_number = e.accession_number AND nr.field = 'non_radiological'))"
        ));
    }
    if filters.precursor {
        wheres.push("(e.asp_value IS NOT NULL OR e.asp_finding IS NOT NULL)".into());
    }
    if let Some(v) = filters.min_ccdp {
        wheres.push("e.asp_value >= ?".into());
        params.push(Value::Real(v));
    }
    if !filters.reported_under.is_empty() {
        let any: Vec<String> = filters.reported_under.iter().map(|c| cites(*c)).collect();
        wheres.push(format!("({})", any.join(" OR ")));
    }
    if !filters.any_barrier.is_empty() {
        let any: Vec<String> = BARRIERS
            .iter()
            .map(|b| format!("e.{b} IN ({})", placeholders(&filters.any_barrier, params)))
            .collect();
        wheres.push(format!("({})", any.join(" OR ")));
    }
    if !wheres.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&wheres.join(" AND "));
    }
    sql
}

/// `?, ?, ?` for `values`, pushing them onto `params`.
fn placeholders<S: AsRef<str>>(values: &[S], params: &mut Vec<Value>) -> String {
    params.extend(values.iter().map(|v| Value::Text(v.as_ref().into())));
    vec!["?"; values.len()].join(", ")
}

/// The stored names of vocabulary values.
fn names<T: AsRef<str>>(values: &[T]) -> Vec<&str> {
    values.iter().map(AsRef::as_ref).collect()
}

/// Runs `query` with the filters' text; if that isn't valid FTS5 syntax (a stray hyphen reads as
/// a column filter), runs it again with each word quoted.
fn with_fts_retry<T>(
    filters: &Filters,
    query: impl Fn(Option<&str>) -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    match query(filters.text.as_deref()) {
        Err(e) if filters.text.is_some() && is_fts_syntax_error(&e) => {
            let quoted = filters.text.as_deref().map(quote_words);
            query(quoted.as_deref())
        }
        other => other,
    }
}

/// SQLITE_ERROR, which is what FTS5 query parse errors come back as ("fts5: syntax error near",
/// "unterminated string", "no such column"). Anything else would fail the retry the same way.
fn is_fts_syntax_error(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::Unknown)
}

/// `aux-feed water` -> `"aux-feed" "water"`: every word literal, all required.
fn quote_words(text: &str) -> String {
    text.split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// EIIS system codes are two letters ("BA", "SJ").
fn is_eiis_code(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic())
}

/// `LIKE '%needle%'` pattern with the wildcards in `needle` escaped (ESCAPE '\').
fn contains_pattern(needle: &str) -> String {
    let escaped = needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

// ---------------------------------------------------------------- search

#[derive(Debug, Clone, Default)]
pub struct Search {
    pub filters: Filters,
    pub sort: Sort,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Sort {
    #[default]
    Newest,
    Oldest,
    /// FTS rank; needs `text`.
    Relevance,
    /// `severity_sql` score, highest first, then newest.
    Severity,
    /// NRC precursor CCDP/delta-CDP, highest first; finding-color-only precursors next.
    Risk,
}

/// Latest revision of each LER (`events`), filtered and sorted.
pub fn search_events(db: &Connection, search: &Search) -> rusqlite::Result<SearchResults> {
    with_fts_retry(&search.filters, |text| run_search(db, search, text))
}

fn run_search(
    db: &Connection,
    search: &Search,
    text: Option<&str>,
) -> rusqlite::Result<SearchResults> {
    let mut params: Vec<Value> = vec![];
    let mut sql = String::from(
        "SELECT e.accession_number, e.ler_number, e.docket, e.site, e.reporting_unit, e.event_date,
                e.report_date, e.title, e.system_name, e.eiis_code, e.primary_cause, e.failure_mode,
                e.human_error, e.summary, e.fuel_cladding, e.rcs_pressure_boundary, e.containment,
                e.radiological_release, e.ler_key, count(*) OVER () AS total",
    );
    sql.push_str(&format!(", {} AS severity", severity_sql()));
    sql.push_str(&from_where(&search.filters, text, &mut params));
    sql.push_str(match search.sort {
        Sort::Newest => {
            " ORDER BY COALESCE(e.event_date, e.report_date) DESC, e.accession_number DESC"
        }
        Sort::Oldest => {
            " ORDER BY COALESCE(e.event_date, e.report_date) ASC, e.accession_number ASC"
        }
        Sort::Relevance => " ORDER BY f.rank",
        Sort::Risk => {
            " ORDER BY e.asp_value DESC NULLS LAST, e.asp_finding IS NULL, \
             COALESCE(e.event_date, e.report_date) DESC, e.accession_number DESC"
        }
        Sort::Severity => {
            " ORDER BY severity DESC NULLS LAST, COALESCE(e.event_date, e.report_date) DESC, \
             e.accession_number DESC"
        }
    });
    sql.push_str(" LIMIT ?");
    params.push(Value::Integer(search.limit as i64));

    let mut total = 0;
    let mut keys: Vec<Option<String>> = vec![];
    let mut results = db
        .prepare(&sql)?
        .query_map(params_from_iter(params), |r| {
            total = r.get::<_, i64>("total")? as usize;
            keys.push(r.get("ler_key")?);
            Ok(EventSummary {
                accession_number: r.get("accession_number")?,
                ler_number: r.get("ler_number")?,
                docket: r.get("docket")?,
                site: r.get("site")?,
                reporting_unit: r.get("reporting_unit")?,
                event_date: r.get("event_date")?,
                report_date: r.get("report_date")?,
                title: r.get("title")?,
                system_name: r.get("system_name")?,
                eiis_code: r.get("eiis_code")?,
                primary_cause: r.get("primary_cause")?,
                failure_mode: r.get("failure_mode")?,
                human_error: r.get("human_error")?,
                who: vec![],
                operator_fundamentals: vec![],
                summary: r.get("summary")?,
                barriers: BARRIERS
                    .iter()
                    .filter_map(|b| match r.get::<_, Option<BarrierState>>(*b) {
                        Ok(Some(v)) if v != BarrierState::None => Some(Ok(format!("{b}: {v}"))),
                        Ok(_) => None,
                        Err(e) => Some(Err(e)),
                    })
                    .collect::<rusqlite::Result<_>>()?,
                radiological_release: r
                    .get::<_, Option<RadiologicalRelease>>("radiological_release")?
                    .filter(|v| *v != RadiologicalRelease::None),
                non_radiological_release: vec![],
                safety_functions_lost: vec![],
                reported_under: vec![],
                precursor: None,
                severity: r.get("severity")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for r in &mut results {
        r.who = tags(db, &r.accession_number, "who")?;
        r.operator_fundamentals = tags(db, &r.accession_number, "operator_fundamentals")?;
        r.safety_functions_lost = tags(db, &r.accession_number, "safety_functions_lost")?;
        r.non_radiological_release = tags(db, &r.accession_number, "non_radiological")?;
        r.reported_under = criteria_cited(db, &r.accession_number)?;
    }
    for (r, key) in results.iter_mut().zip(&keys) {
        if let Some(k) = key {
            r.precursor = precursor(db, k)?;
        }
    }
    Ok(SearchResults {
        total,
        results,
        notes: vec![],
    })
}

// ---------------------------------------------------------------- summarize

/// What `summarize_events` counts by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GroupBy {
    Cause,
    HumanError,
    ErrorType,
    /// Multi-valued (class_tags): an event counts once under each group named.
    Who,
    /// Multi-valued (class_tags).
    OperatorFundamentals,
    FailureMode,
    FailureMechanism,
    /// EIIS system code.
    System,
    Plant,
    ReactorType,
    Year,
    Decade,
    FuelCladding,
    RcsPressureBoundary,
    Containment,
    RadiologicalRelease,
    /// Multi-valued (class_tags).
    NonRadiologicalRelease,
    /// Multi-valued (class_tags).
    SafetyFunctionsLost,
}

impl GroupBy {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cause => "cause",
            Self::HumanError => "human_error",
            Self::ErrorType => "error_type",
            Self::Who => "who",
            Self::OperatorFundamentals => "operator_fundamentals",
            Self::FailureMode => "failure_mode",
            Self::FailureMechanism => "failure_mechanism",
            Self::System => "system",
            Self::Plant => "plant",
            Self::ReactorType => "reactor_type",
            Self::Year => "year",
            Self::Decade => "decade",
            Self::FuelCladding => "fuel_cladding",
            Self::RcsPressureBoundary => "rcs_pressure_boundary",
            Self::Containment => "containment",
            Self::RadiologicalRelease => "radiological_release",
            Self::NonRadiologicalRelease => "non_radiological_release",
            Self::SafetyFunctionsLost => "safety_functions_lost",
        }
    }

    /// class_tags field for multi-valued groups.
    fn tag_field(self) -> Option<&'static str> {
        match self {
            Self::Who => Some("who"),
            Self::OperatorFundamentals => Some("operator_fundamentals"),
            Self::SafetyFunctionsLost => Some("safety_functions_lost"),
            Self::NonRadiologicalRelease => Some("non_radiological"),
            _ => None,
        }
    }

    /// SQL for this group's value of one events row; multi-valued groups read tag alias `t`.
    fn column(self, t: &str) -> String {
        match self {
            Self::Cause => "e.primary_cause".into(),
            Self::HumanError => "e.human_error".into(),
            Self::ErrorType => "e.error_type".into(),
            Self::FailureMode => "e.failure_mode".into(),
            Self::FailureMechanism => "e.failure_mechanism".into(),
            Self::System => "e.eiis_code".into(),
            Self::Plant => "e.site".into(),
            Self::ReactorType => "e.reactor_type".into(),
            Self::Year => "substr(COALESCE(e.event_date, e.report_date), 1, 4)".into(),
            Self::Decade => "substr(COALESCE(e.event_date, e.report_date), 1, 3) || '0s'".into(),
            Self::FuelCladding => "e.fuel_cladding".into(),
            Self::RcsPressureBoundary => "e.rcs_pressure_boundary".into(),
            Self::Containment => "e.containment".into(),
            Self::RadiologicalRelease => "e.radiological_release".into(),
            Self::Who
            | Self::OperatorFundamentals
            | Self::SafetyFunctionsLost
            | Self::NonRadiologicalRelease => {
                format!("{t}.value")
            }
        }
    }

    /// The tag join for a multi-valued group, as alias `t`; LEFT so events without tags stay
    /// (value NULL). Empty for single-valued groups.
    fn join(self, t: &str) -> String {
        match self.tag_field() {
            Some(field) => format!(
                " LEFT JOIN class_tags {t} ON {t}.accession_number = e.accession_number \
                 AND {t}.field = '{field}'"
            ),
            None => String::new(),
        }
    }

    /// Time groups read better in order; the rest by count.
    fn chronological(self) -> bool {
        matches!(self, Self::Year | Self::Decade)
    }
}

#[derive(Debug, Clone)]
pub struct Summarize {
    pub filters: Filters,
    pub group_by: GroupBy,
    /// Second field: each group is broken down by it, as a share of that group.
    pub then_by: Option<GroupBy>,
    /// Most groups returned (per level); the rest are counted in `other_groups` / `more`.
    pub top: usize,
}

/// Event counts per group for the events matching the filters.
pub fn summarize_events(db: &Connection, s: &Summarize) -> rusqlite::Result<Breakdown> {
    with_fts_retry(&s.filters, |text| run_summarize(db, s, text))
}

type Counts = Vec<(Option<String>, i64)>;
/// (group value, then_by value, events) rows.
type PairCounts = Vec<(Option<String>, Option<String>, i64)>;

/// Time groups by value (no date last), the rest by count, ties by value.
fn sort_counts(counts: &mut Counts, chronological: bool) {
    if chronological {
        counts.sort_by(|a, b| (a.0.is_none(), &a.0).cmp(&(b.0.is_none(), &b.0)));
    } else {
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }
}

/// Percent of `of`, one decimal.
fn pct(n: i64, of: i64) -> f64 {
    if of == 0 {
        0.0
    } else {
        (1000.0 * n as f64 / of as f64).round() / 10.0
    }
}

fn run_summarize(
    db: &Connection,
    s: &Summarize,
    text: Option<&str>,
) -> rusqlite::Result<Breakdown> {
    let mut params: Vec<Value> = vec![];
    let from = from_where(&s.filters, text, &mut params);
    let with_joins =
        |joins: &str| from.replacen(" FROM events e", &format!(" FROM events e{joins}"), 1);
    let query = |sql: &str| -> rusqlite::Result<PairCounts> {
        db.prepare(sql)?
            .query_map(params_from_iter(params.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect()
    };
    let total: i64 = db.query_row(
        &format!("SELECT count(*){from}"),
        params_from_iter(params.iter()),
        |r| r.get(0),
    )?;

    // DISTINCT: with a tag join, one event appears once per tag.
    let g = s.group_by;
    let mut groups: Counts = query(&format!(
        "SELECT {} AS value, NULL, count(DISTINCT e.accession_number){} GROUP BY 1",
        g.column("t1"),
        with_joins(&g.join("t1"))
    ))?
    .into_iter()
    .map(|(v, _, n)| (v, n))
    .collect();
    sort_counts(&mut groups, g.chronological());

    let mut subs: std::collections::HashMap<Option<String>, Counts> = Default::default();
    if let Some(t) = s.then_by {
        let pairs = query(&format!(
            "SELECT {}, {}, count(DISTINCT e.accession_number){} GROUP BY 1, 2",
            g.column("t1"),
            t.column("t2"),
            with_joins(&(g.join("t1") + &t.join("t2")))
        ))?;
        for (v, sub, n) in pairs {
            subs.entry(v).or_default().push((sub, n));
        }
        for counts in subs.values_mut() {
            sort_counts(counts, t.chronological());
        }
    }

    let other_groups = groups.len().saturating_sub(s.top);
    let other_events: i64 = groups.iter().skip(s.top).map(|(_, n)| n).sum();
    let groups = groups
        .into_iter()
        .take(s.top)
        .map(|(value, n)| {
            let sub = subs.remove(&value).unwrap_or_default();
            GroupCount {
                more: sub.len().saturating_sub(s.top),
                by: sub
                    .into_iter()
                    .take(s.top)
                    .map(|(v, m)| GroupCount {
                        value: v,
                        events: m as usize,
                        pct: pct(m, n),
                        by: vec![],
                        more: 0,
                    })
                    .collect(),
                value,
                events: n as usize,
                pct: pct(n, total),
            }
        })
        .collect();
    Ok(Breakdown {
        notes: vec![],
        group_by: g.name().into(),
        then_by: s.then_by.map(|t| t.name().into()),
        multi_valued: g.tag_field().is_some() || s.then_by.is_some_and(|t| t.tag_field().is_some()),
        total_events: total as usize,
        groups,
        other_groups,
        other_events: other_events as usize,
    })
}

// ---------------------------------------------------------------- queries

/// Resolve an id to matching documents, then load the report or list the candidates.
pub fn find_event(db: &Connection, id: &EventId) -> rusqlite::Result<Lookup<EventReport>> {
    resolve(db, id, |acc| {
        let Some(mut report) = load_report(db, acc)? else {
            return Ok(None);
        };
        add_notes(db, &mut report)?;
        Ok(Some(report))
    })
}

/// One match: `load` it; several: list them; none: NotFound.
fn resolve<T>(
    db: &Connection,
    id: &EventId,
    load: impl FnOnce(&str) -> rusqlite::Result<Option<T>>,
) -> rusqlite::Result<Lookup<T>> {
    let accessions = match_accessions(db, id)?;
    Ok(match accessions.as_slice() {
        [] => Lookup::NotFound,
        [acc] => load(acc)?.map_or(Lookup::NotFound, |t| Lookup::Found(Box::new(t))),
        many => Lookup::Ambiguous(Candidates {
            shown: load_candidates(db, &many[..many.len().min(MAX_CANDIDATES)])?,
            total: many.len(),
        }),
    })
}

// ---------------------------------------------------------------- report text

/// Parts of an LER's text, as split by the parser. `Narrative` is the whole continuation-sheet
/// text, for reports whose sections couldn't be found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Section {
    Abstract,
    ConditionPrior,
    Description,
    Cause,
    Safety,
    Corrective,
    Previous,
    Narrative,
}

impl Section {
    pub const DEFAULT: &[Section] = &[
        Section::Abstract,
        Section::Description,
        Section::Cause,
        Section::Corrective,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Abstract => "abstract",
            Self::ConditionPrior => "condition_prior",
            Self::Description => "description",
            Self::Cause => "cause",
            Self::Safety => "safety",
            Self::Corrective => "corrective",
            Self::Previous => "previous",
            Self::Narrative => "narrative",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Self::Abstract => "abstract",
            Self::ConditionPrior => "sec_condition_prior",
            Self::Description => "sec_description",
            Self::Cause => "sec_cause",
            Self::Safety => "sec_safety",
            Self::Corrective => "sec_corrective",
            Self::Previous => "sec_previous",
            Self::Narrative => "narrative",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextRequest {
    pub sections: Vec<Section>,
    /// Total characters returned across sections; later sections get what earlier ones leave.
    pub max_chars: usize,
}

pub fn find_text(
    db: &Connection,
    id: &EventId,
    req: &TextRequest,
) -> rusqlite::Result<Lookup<EventText>> {
    resolve(db, id, |acc| load_text(db, acc, req))
}

fn load_text(db: &Connection, acc: &str, req: &TextRequest) -> rusqlite::Result<Option<EventText>> {
    let cols: Vec<&str> = req.sections.iter().map(|s| s.column()).collect();
    let sql = format!(
        "SELECT l.accession_number, l.ler_number, l.docket, u.name, COALESCE(t.title, l.title),
                l.event_date, l.pdf_url, l.narrative IS NOT NULL{}
         FROM lers l LEFT JOIN units u ON u.docket = l.docket
         LEFT JOIN adams_titles t ON t.accession_number = l.accession_number
         WHERE l.accession_number = ?1",
        cols.iter().map(|c| format!(", l.{c}")).collect::<String>()
    );
    let row = db
        .prepare(&sql)?
        .query_row([acc], |r| {
            let texts: Vec<Option<String>> = (0..cols.len())
                .map(|i| r.get(8 + i))
                .collect::<rusqlite::Result<_>>()?;
            Ok((
                EventText {
                    accession_number: r.get(0)?,
                    ler_number: r.get(1)?,
                    docket: r.get(2)?,
                    reporting_unit: r.get(3)?,
                    title: r.get(4)?,
                    event_date: r.get(5)?,
                    pdf_url: r.get(6)?,
                    sections: vec![],
                    missing: vec![],
                },
                r.get::<_, bool>(7)?,
                texts,
            ))
        })
        .optional()?;
    let Some((mut out, has_narrative, texts)) = row else {
        return Ok(None);
    };
    let mut found: Vec<(Section, String)> = vec![];
    for (sec, text) in req.sections.iter().zip(texts) {
        match text.filter(|t| !t.trim().is_empty()) {
            Some(t) => found.push((*sec, t)),
            None => out.missing.push(sec.name().into()),
        }
    }
    // Nothing split out (older or unusual reports): give the whole narrative instead.
    if found.is_empty() && has_narrative && !req.sections.contains(&Section::Narrative) {
        let narrative: Option<String> = db.query_row(
            "SELECT narrative FROM lers WHERE accession_number = ?1",
            [acc],
            |r| r.get(0),
        )?;
        found.extend(narrative.map(|n| (Section::Narrative, n)));
    }
    let mut budget = req.max_chars;
    for (sec, text) in found {
        let chars = text.chars().count();
        let keep = chars.min(budget);
        budget -= keep;
        out.sections.push(TextSection {
            name: sec.name().into(),
            text: text.chars().take(keep).collect(),
            chars,
            truncated: keep < chars,
        });
    }
    Ok(Some(out))
}

/// Accession numbers matching the id. Without a revision, only the latest revision of each LER
/// (`ler_latest`) is considered; with one, that exact revision (`lers.ler_rev`).
fn match_accessions(db: &Connection, id: &EventId) -> rusqlite::Result<Vec<String>> {
    let collect = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<Vec<String>> {
        db.prepare_cached(sql)?
            .query_map(p, |r| r.get(0))?
            .collect()
    };
    match id {
        EventId::Accession(acc) => collect(
            "SELECT accession_number FROM lers WHERE accession_number = ?1",
            &[acc],
        ),
        EventId::Ler(LerRef::Full(n)) => match n.sequence.revision {
            None => collect(
                "SELECT accession_number FROM ler_latest WHERE ler_key = ?1",
                &[n],
            ),
            Some(rev) => collect(
                "SELECT accession_number FROM lers WHERE ler_key = ?1 AND ler_rev = ?2 ORDER BY document_date DESC LIMIT 1",
                &[n, &rev],
            ),
        },
        EventId::Ler(LerRef::AnyUnit(s)) => match s.revision {
            None => collect(
                "SELECT accession_number FROM ler_latest WHERE substr(ler_number, 1, 8) = ?1 ORDER BY docket",
                &[s],
            ),
            Some(rev) => collect(
                "SELECT accession_number FROM lers WHERE substr(ler_number, 1, 8) = ?1 AND ler_rev = ?2 ORDER BY docket",
                &[s, &rev],
            ),
        },
    }
}

const REPORT_SQL: &str = "
    SELECT l.accession_number, l.ler_number, l.docket, COALESCE(t.title, l.title) AS title,
           s.name AS site, u.name AS reporting_unit, l.reactor_type, l.event_date, l.report_date, l.pdf_url,
           c.accession_number IS NOT NULL AS classified,
           c.summary, c.primary_cause, c.human_error, c.error_type, c.system_name, c.eiis_code, c.component,
           c.failure_mode, c.failure_mechanism, c.fuel_cladding, c.rcs_pressure_boundary, c.containment,
           c.radiological_release, c.release_pathway, c.non_radiological_description, c.trip_risk,
           c.confidence, c.prompt_version, c.model, l.ler_key
    FROM lers l
    LEFT JOIN units u ON u.docket = l.docket
    LEFT JOIN sites s ON s.site_id = l.site_id
    LEFT JOIN adams_titles t ON t.accession_number = l.accession_number
    LEFT JOIN classification c ON c.accession_number = l.accession_number
    WHERE l.accession_number = ?1";

fn load_report(db: &Connection, acc: &str) -> rusqlite::Result<Option<EventReport>> {
    let Some(mut report) = db
        .prepare_cached(REPORT_SQL)?
        .query_row([acc], report_from_row)
        .optional()?
    else {
        return Ok(None);
    };
    let key: Option<String> = db.query_row(
        "SELECT ler_key FROM lers WHERE accession_number = ?1",
        [acc],
        |r| r.get(0),
    )?;
    if let Some(k) = key {
        report.precursor = precursor(db, &k)?;
    }
    if let Some(c) = report.classification.as_mut() {
        c.who = tags(db, acc, "who")?;
        c.operator_fundamentals = tags(db, acc, "operator_fundamentals")?;
        c.non_radiological_release = tags(db, acc, "non_radiological")?;
        c.generation = generation(db, acc)?;
    }
    Ok(Some(report))
}

/// Maps one REPORT_SQL row by column name. List fields are filled in by `load_report`.
fn report_from_row(r: &Row) -> rusqlite::Result<EventReport> {
    let classification = if r.get("classified")? {
        Some(Classification {
            summary: r.get("summary")?,
            primary_cause: r.get("primary_cause")?,
            human_error: r.get("human_error")?,
            error_type: r.get("error_type")?,
            who: vec![],
            operator_fundamentals: vec![],
            system_name: r.get("system_name")?,
            eiis_code: r.get("eiis_code")?,
            component: r.get("component")?,
            failure_mode: r.get("failure_mode")?,
            failure_mechanism: r.get("failure_mechanism")?,
            barriers: Barriers {
                fuel_cladding: r.get("fuel_cladding")?,
                rcs_pressure_boundary: r.get("rcs_pressure_boundary")?,
                containment: r.get("containment")?,
            },
            radiological_release: r.get("radiological_release")?,
            release_pathway: r.get("release_pathway")?,
            non_radiological_release: vec![],
            non_radiological_description: r.get("non_radiological_description")?,
            trip_risk: r.get("trip_risk")?,
            generation: vec![],
            confidence: r.get("confidence")?,
            source: format!(
                "{} / {}",
                r.get::<_, String>("prompt_version")?,
                r.get::<_, String>("model")?
            ),
        })
    } else {
        None
    };
    Ok(EventReport {
        precursor: None,
        accession_number: r.get("accession_number")?,
        ler_number: r.get("ler_number")?,
        docket: r.get("docket")?,
        title: r.get("title")?,
        site: r.get("site")?,
        reporting_unit: r.get("reporting_unit")?,
        reactor_type: r.get("reactor_type")?,
        event_date: r.get("event_date")?,
        report_date: r.get("report_date")?,
        pdf_url: r.get("pdf_url")?,
        classification,
        notes: vec![],
    })
}

/// The NRC precursor result for an LER key, if the NRC rated it a precursor. Tolerates a
/// database built before ASP results were added (no table).
fn precursor(db: &Connection, ler_key: &str) -> rusqlite::Result<Option<Precursor>> {
    let row = db
        .prepare_cached(
            "SELECT value, finding, significant, n_sources, values_seen FROM asp_precursors WHERE ler_key = ?1",
        )
        .and_then(|mut st| {
            st.query_row([ler_key], |r| {
                Ok((
                    r.get::<_, Option<f64>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .optional()
        });
    let row = match row {
        Err(rusqlite::Error::SqliteFailure(_, Some(m))) if m.contains("no such table") => {
            return Ok(None);
        }
        other => other?,
    };
    Ok(row.map(|(ccdp, finding, significant, sources, seen)| {
        precursor_from(ccdp, finding, significant, sources, seen)
    }))
}

/// A `Precursor` from asp_precursors columns; `seen` is the values_seen JSON ({"6e-3": n}).
fn precursor_from(
    ccdp: Option<f64>,
    finding: Option<String>,
    significant: bool,
    sources: i64,
    seen: Option<String>,
) -> Precursor {
    let mut other_estimates: Vec<f64> = seen
        .and_then(|j| serde_json::from_str::<std::collections::BTreeMap<String, i64>>(&j).ok())
        .map(|m| m.keys().filter_map(|k| k.parse().ok()).collect())
        .unwrap_or_default();
    other_estimates.retain(|v| Some(*v) != ccdp);
    other_estimates.sort_by(|a, b| b.total_cmp(a));
    Precursor {
        ccdp,
        finding,
        significant,
        sources,
        other_estimates,
    }
}

// ---------------------------------------------------------------- precursors

#[derive(Debug, Clone, Default)]
pub struct PrecursorQuery {
    /// Reporting dockets (from the plant filter).
    pub dockets: Option<Vec<String>>,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub min_ccdp: Option<f64>,
    pub limit: usize,
}

/// NRC precursors, riskiest first, linked to the LER in this database when it is here.
pub fn list_precursors(db: &Connection, q: &PrecursorQuery) -> rusqlite::Result<PrecursorList> {
    let mut wheres = vec!["1".to_string()];
    let mut params: Vec<Value> = vec![];
    if let Some(d) = &q.dockets {
        wheres.push(format!("p.docket IN ({})", placeholders(d, &mut params)));
    }
    let date = "COALESCE(p.event_date, p.ler_year || '')";
    if let Some(d) = &q.date_from {
        wheres.push(format!("{date} >= ?"));
        params.push(Value::Text(d.clone()));
    }
    if let Some(d) = &q.date_to {
        wheres.push(format!("substr({date}, 1, {}) <= ?", d.len()));
        params.push(Value::Text(d.clone()));
    }
    if let Some(v) = q.min_ccdp {
        wheres.push("p.value >= ?".into());
        params.push(Value::Real(v));
    }
    let sql = format!(
        "SELECT p.docket, p.ler_year, p.ler_seq, p.value, p.finding, p.significant, p.n_sources,
                p.values_seen, p.event_date, COALESCE(u.name, p.plant) AS plant, e.accession_number, e.title,
                count(*) OVER () AS total
         FROM asp_precursors p
         LEFT JOIN units u ON u.docket = p.docket
         LEFT JOIN events e ON e.ler_key = p.ler_key
         WHERE {}
         ORDER BY p.value DESC NULLS LAST, p.event_date DESC
         LIMIT ?",
        wheres.join(" AND ")
    );
    params.push(Value::Integer(q.limit as i64));
    let mut total = 0;
    let results = db
        .prepare(&sql)?
        .query_map(params_from_iter(params), |r| {
            total = r.get::<_, i64>("total")? as usize;
            Ok(PrecursorEntry {
                ler: format!(
                    "{}/{:04}-{:03}",
                    r.get::<_, String>("docket")?,
                    r.get::<_, i64>("ler_year")?,
                    r.get::<_, i64>("ler_seq")?
                ),
                plant: r.get("plant")?,
                event_date: r.get("event_date")?,
                precursor: precursor_from(
                    r.get("value")?,
                    r.get("finding")?,
                    r.get("significant")?,
                    r.get("n_sources")?,
                    r.get("values_seen")?,
                ),
                accession_number: r.get("accession_number")?,
                title: r.get("title")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(PrecursorList {
        notes: vec![],
        total,
        results,
    })
}

/// The values of a multi-valued classification field, in the classifier's order.
fn tags<T: FromSql>(db: &Connection, acc: &str, field: &str) -> rusqlite::Result<Vec<T>> {
    db.prepare_cached(
        "SELECT value FROM class_tags WHERE accession_number = ?1 AND field = ?2 ORDER BY rowid",
    )?
    .query_map([acc, field], |r| r.get(0))?
    .collect()
}

fn generation(db: &Connection, acc: &str) -> rusqlite::Result<Vec<UnitImpact>> {
    db.prepare_cached(
        "SELECT unit_name, docket, event_occurred_here, impact, power_before_pct, power_after_pct, duration_hours
         FROM class_generation WHERE accession_number = ?1 ORDER BY event_occurred_here DESC, rowid",
    )?
    .query_map([acc], |r| {
        Ok(UnitImpact {
            unit_name: r.get("unit_name")?,
            docket: r.get("docket")?,
            event_occurred_here: r.get("event_occurred_here")?,
            impact: r.get("impact")?,
            power_before_pct: r.get("power_before_pct")?,
            power_after_pct: r.get("power_after_pct")?,
            duration_hours: r.get("duration_hours")?,
        })
    })?
    .collect()
}

/// Notes for the reader: a newer revision exists, or why there's no classification.
fn add_notes(db: &Connection, report: &mut EventReport) -> rusqlite::Result<()> {
    let latest: Option<(String, Option<String>)> = db
        .prepare_cached(
            "SELECT latest.accession_number, latest.ler_number
             FROM lers l JOIN ler_latest latest ON latest.ler_key = l.ler_key
             WHERE l.accession_number = ?1",
        )?
        .query_row([&report.accession_number], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    if let Some((acc, num)) = latest
        && acc != report.accession_number
    {
        report.notes.push(format!(
            "Superseded by {} ({acc}). Classifications exist only for the latest revision.",
            num.as_deref().unwrap_or("a later revision")
        ));
    }
    if report.classification.is_none() && report.notes.is_empty() {
        report.notes.push(
            "Not classified: only the latest revision of power reactor LERs with at least 500 characters of text were."
                .into(),
        );
    }
    Ok(())
}

fn load_candidates(db: &Connection, accessions: &[String]) -> rusqlite::Result<Vec<Candidate>> {
    let mut stmt = db.prepare_cached(
        "SELECT l.accession_number, l.ler_number, l.docket, u.name AS reporting_unit, COALESCE(t.title, l.title) AS title
         FROM lers l LEFT JOIN units u ON u.docket = l.docket
         LEFT JOIN adams_titles t ON t.accession_number = l.accession_number
         WHERE l.accession_number = ?1",
    )?;
    accessions
        .iter()
        .map(|acc| {
            stmt.query_row([acc], |r| {
                Ok(Candidate {
                    accession_number: r.get("accession_number")?,
                    ler_number: r.get("ler_number")?,
                    docket: r.get("docket")?,
                    reporting_unit: r.get("reporting_unit")?,
                    title: r.get("title")?,
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The subset of lers_analysis.db the queries touch, with the real ler_latest definition.
    const FIXTURE: &str = "
        CREATE TABLE lers (accession_number TEXT PRIMARY KEY, ler_number TEXT, ler_rev INTEGER, ler_key TEXT,
            docket TEXT, site_id TEXT, title TEXT, event_date TEXT, report_date TEXT, document_date TEXT,
            reactor_type TEXT, pdf_url TEXT);
        CREATE TABLE units (docket TEXT PRIMARY KEY, site_id TEXT, unit TEXT, name TEXT);
        CREATE TABLE sites (site_id TEXT PRIMARY KEY, name TEXT, state TEXT);
        CREATE TABLE adams_titles (accession_number TEXT PRIMARY KEY, title TEXT);
        CREATE TABLE classification (accession_number TEXT PRIMARY KEY, prompt_version TEXT, model TEXT,
            summary TEXT, primary_cause TEXT, human_error TEXT, error_type TEXT, eiis_code TEXT, system_name TEXT,
            component TEXT, failure_mode TEXT, failure_mechanism TEXT, fuel_cladding TEXT,
            rcs_pressure_boundary TEXT, containment TEXT, radiological_release TEXT, trip_risk TEXT, confidence TEXT,
            release_pathway TEXT, non_radiological_description TEXT);
        CREATE TABLE class_generation (accession_number TEXT, unit_name TEXT, docket TEXT, event_occurred_here INTEGER,
            impact TEXT, power_before_pct INTEGER, power_after_pct INTEGER, duration_hours REAL);
        CREATE TABLE class_tags (accession_number TEXT, field TEXT, value TEXT);
        CREATE VIEW ler_latest AS
          SELECT l.* FROM lers l
          WHERE l.ler_key IS NOT NULL AND l.accession_number = (
             SELECT l2.accession_number FROM lers l2 WHERE l2.ler_key = l.ler_key
             ORDER BY l2.ler_rev DESC, l2.document_date DESC, l2.accession_number DESC LIMIT 1);

        INSERT INTO sites VALUES ('sequoyah', 'Sequoyah', 'TN');
        INSERT INTO units VALUES ('05000327', 'sequoyah', '1', 'Sequoyah Unit 1'),
                                 ('05000328', 'sequoyah', '2', 'Sequoyah Unit 2');
        INSERT INTO lers VALUES
          ('ML14219A568', '2014-002-00', 0, '05000327:2014-002', '05000327', 'sequoyah', 'U1 title',
           '2014-06-06', '2014-08-05', '2014-08-05', 'PWR', NULL),
          ('ML14200A001', '2014-002-00', 0, '05000328:2014-002', '05000328', 'sequoyah', 'U2 rev 0',
           '2014-05-01', '2014-07-01', '2014-07-01', 'PWR', NULL),
          ('ML14300A002', '2014-002-01', 1, '05000328:2014-002', '05000328', 'sequoyah', 'U2 rev 1',
           '2014-05-01', '2014-10-01', '2014-10-01', 'PWR', NULL);
        INSERT INTO adams_titles VALUES ('ML14219A568', 'Clean U1 title');
        INSERT INTO classification VALUES ('ML14219A568', 'p4', 'claude-haiku-5-5', 'Summary', 'procedure',
            'contributing', 'procedure_inadequate', 'NH', 'containment isolation', NULL, 'no_equipment_failure',
            NULL, 'none', 'none', 'degraded', 'none', 'none', 'high', NULL, NULL);
        INSERT INTO class_generation VALUES
          ('ML14219A568', 'Sequoyah Unit 1', '05000327', 0, 'none', NULL, NULL, NULL),
          ('ML14219A568', 'Sequoyah Unit 2', '05000328', 1, 'already_shutdown', 0, 0, NULL);
        INSERT INTO class_tags VALUES ('ML14219A568', 'who', 'maintenance'),
                                      ('ML14219A568', 'who', 'engineering'),
                                      ('ML14219A568', 'safety_functions_lost', 'release_control');

        CREATE TABLE asp_precursors (docket TEXT, ler_year INTEGER, ler_seq INTEGER, ler_key TEXT PRIMARY KEY,
            value REAL, finding TEXT, significant INTEGER, event_date TEXT, plant TEXT, description TEXT,
            n_sources INTEGER, n_agree INTEGER, values_seen TEXT, sources TEXT);
        INSERT INTO asp_precursors VALUES ('05000327', 1975, 6, '05000327:1975-006', 0.4, NULL, 1,
            '1975-03-22', NULL, NULL, 2, 2, NULL, '[]');
        INSERT INTO asp_precursors VALUES ('05000328', 2014, 2, '05000328:2014-002', 0.002, NULL, 1,
            '2014-05-01', NULL, NULL, 3, 2, '{\"0.002\": 2, \"0.01\": 1}', '[]');
        CREATE TABLE ler_cfr (accession_number TEXT, cfr_section TEXT);
        INSERT INTO ler_cfr VALUES ('ML14219A568', '50.73(a)(2)(i)(B)'), ('ML14219A568', '50.73(a)(2)(v)(C)'),
                                   ('ML14200A001', '50.73(a)(2)(vii)');
        ALTER TABLE lers ADD COLUMN date_added TEXT;
        UPDATE lers SET date_added = '2014-10-02 09:00' WHERE accession_number = 'ML14300A002';
        CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT);
        INSERT INTO meta VALUES ('built_at', '2026-11-15T14:35:09'), ('parser_version', '5');
        ALTER TABLE lers ADD COLUMN abstract TEXT;
        ALTER TABLE lers ADD COLUMN narrative TEXT;
        ALTER TABLE lers ADD COLUMN sec_condition_prior TEXT;
        ALTER TABLE lers ADD COLUMN sec_description TEXT;
        ALTER TABLE lers ADD COLUMN sec_cause TEXT;
        ALTER TABLE lers ADD COLUMN sec_safety TEXT;
        ALTER TABLE lers ADD COLUMN sec_corrective TEXT;
        ALTER TABLE lers ADD COLUMN sec_previous TEXT;
        UPDATE lers SET abstract = 'Penetrations were opened.', narrative = 'Full narrative text.',
            sec_description = 'The penetrations were opened during fuel movement.',
            sec_cause = 'An inadequate procedure revision.'
          WHERE accession_number = 'ML14219A568';
        UPDATE lers SET narrative = 'Only an unsplit narrative here.' WHERE accession_number = 'ML14300A002';

        -- The columns of the real events view that search uses.
        CREATE VIEW events AS
          SELECT l.accession_number, l.ler_number, l.event_date, l.report_date, s.name AS site,
                 u.name AS reporting_unit, l.docket, COALESCE(t.title, l.title) AS title, c.summary,
                 c.primary_cause, c.human_error, c.error_type, c.eiis_code, c.system_name, c.failure_mode,
                 c.failure_mechanism, l.reactor_type, c.fuel_cladding, c.rcs_pressure_boundary,
                 c.containment, c.radiological_release, l.ler_key, a.value AS asp_value,
                 a.finding AS asp_finding
          FROM ler_latest l
          LEFT JOIN units u ON u.docket = l.docket
          LEFT JOIN sites s ON s.site_id = l.site_id
          LEFT JOIN adams_titles t ON t.accession_number = l.accession_number
          LEFT JOIN classification c ON c.accession_number = l.accession_number
          LEFT JOIN asp_precursors a ON a.ler_key = l.ler_key;
        CREATE VIRTUAL TABLE lers_fts USING fts5(
          accession_number UNINDEXED, title, abstract, narrative, tokenize='porter');
        INSERT INTO lers_fts VALUES
          ('ML14219A568', 'U1 title', 'Containment penetrations open', 'during fuel movement'),
          ('ML14200A001', 'U2 rev 0', 'auxiliary feedwater pump tripped', ''),
          ('ML14300A002', 'U2 rev 1', 'auxiliary feedwater pump tripped', 'turbine-driven pump');
    ";

    fn fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(FIXTURE).unwrap();
        db
    }

    #[test]
    fn data_info_reads_counts_dates_and_meta() {
        let db = fixture();
        let d = data_info(&db).unwrap();
        assert_eq!((d.events, d.classified), (2, 1));
        assert_eq!(d.newest_report.as_deref(), Some("2014-10-01"));
        assert_eq!(d.newest_added.as_deref(), Some("2014-10-02 09:00"));
        assert_eq!(d.parser_version.as_deref(), Some("5"));
        assert_eq!(d.coverage.by_year, [(2014, 2)]);
        assert_eq!(d.coverage.current_year, Some(2014));
        db.execute_batch("DROP TABLE meta").unwrap();
        assert_eq!(data_info(&db).unwrap().built_at, None, "meta is optional");
    }

    fn find(db: &Connection, id: &str) -> Lookup<EventReport> {
        find_event(db, &id.parse().unwrap()).unwrap()
    }

    fn found<T: std::fmt::Debug>(l: Lookup<T>) -> T {
        match l {
            Lookup::Found(r) => *r,
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn by_accession_loads_everything() {
        let r = found(find(&fixture(), "ml14219a568"));
        assert_eq!(
            r.title.as_deref(),
            Some("Clean U1 title"),
            "ADAMS title preferred"
        );
        assert_eq!(r.reporting_unit.as_deref(), Some("Sequoyah Unit 1"));
        let c = r.classification.unwrap();
        assert_eq!(c.source, "p4 / claude-haiku-5-5");
        assert_eq!(c.barriers.containment, Some(BarrierState::Degraded));
        assert_eq!(c.who, [Who::Maintenance, Who::Engineering]);
        assert!(c.operator_fundamentals.is_empty());
        let units: Vec<_> = c
            .generation
            .iter()
            .map(|u| (u.unit_name.as_str(), u.event_occurred_here))
            .collect();
        assert_eq!(
            units,
            [("Sequoyah Unit 2", true), ("Sequoyah Unit 1", false)],
            "event unit first"
        );
        assert!(r.notes.is_empty());
    }

    #[test]
    fn by_full_ler_number_picks_that_unit() {
        let r = found(find(&fixture(), "327-2014-002"));
        assert_eq!(r.accession_number, "ML14219A568");
    }

    #[test]
    fn full_ler_without_revision_is_latest() {
        let r = found(find(&fixture(), "328-2014-002"));
        assert_eq!(r.accession_number, "ML14300A002");
        assert_eq!(r.notes.len(), 1, "unclassified note: {:?}", r.notes);
        assert!(r.notes[0].starts_with("Not classified"));
    }

    #[test]
    fn explicit_old_revision_is_flagged_superseded() {
        let r = found(find(&fixture(), "328-2014-002-00"));
        assert_eq!(r.accession_number, "ML14200A001");
        assert!(r.classification.is_none());
        assert_eq!(
            r.notes,
            [
                "Superseded by 2014-002-01 (ML14300A002). Classifications exist only for the latest revision."
            ]
        );
    }

    #[test]
    fn sequence_without_docket_is_ambiguous() {
        let Lookup::Ambiguous(c) = find(&fixture(), "2014-002") else {
            panic!()
        };
        assert_eq!(c.total, 2);
        let got: Vec<_> = c
            .shown
            .iter()
            .map(|c| (c.accession_number.as_str(), c.reporting_unit.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("ML14219A568", Some("Sequoyah Unit 1")),
                ("ML14300A002", Some("Sequoyah Unit 2"))
            ]
        );
    }

    #[test]
    fn candidates_are_capped() {
        let db = fixture();
        for n in 0..30 {
            db.execute(
                "INSERT INTO lers (accession_number, ler_number, ler_rev, ler_key, docket)
                 VALUES (?1, '2020-001-00', 0, ?2, ?3)",
                [
                    format!("ML20000A{n:03}"),
                    format!("0500{:04}:2020-001", 100 + n),
                    format!("0500{:04}", 100 + n),
                ],
            )
            .unwrap();
        }
        let Lookup::Ambiguous(c) = find(&db, "2020-001") else {
            panic!()
        };
        assert_eq!((c.shown.len(), c.total), (MAX_CANDIDATES, 30));
    }

    #[test]
    fn sequence_with_revision_can_be_unique() {
        let r = found(find(&fixture(), "2014-002-01"));
        assert_eq!(r.accession_number, "ML14300A002");
    }

    /// Vocabulary values from their stored names.
    fn terms<T: serde::de::DeserializeOwned>(names: &[&str]) -> Vec<T> {
        names
            .iter()
            .map(|n| serde_json::from_value((*n).into()).unwrap())
            .collect()
    }

    fn search(db: &Connection, filters: Filters) -> Vec<String> {
        search_sorted(db, filters, Sort::Newest)
    }

    #[test]
    fn precursor_filter_sort_and_report() {
        let db = fixture();
        let only = Filters {
            precursor: true,
            ..Default::default()
        };
        assert_eq!(search_sorted(&db, only, Sort::Newest), ["ML14300A002"]);
        let high = Filters {
            min_ccdp: Some(1e-2),
            ..Default::default()
        };
        assert!(search_sorted(&db, high, Sort::Newest).is_empty());
        assert_eq!(
            search_sorted(&db, Filters::default(), Sort::Risk)[0],
            "ML14300A002",
            "rated precursor first"
        );
        let hit = search_events(
            &db,
            &Search {
                sort: Sort::Risk,
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap()
        .results
        .remove(0);
        let p = hit.precursor.unwrap();
        assert_eq!((p.ccdp, p.significant, p.sources), (Some(0.002), true, 3));
        assert_eq!(p.other_estimates, [0.01]);
        // get_event of the older revision of the same LER shows it too.
        let r = found(find(&db, "ML14200A001"));
        assert_eq!(r.precursor.unwrap().ccdp, Some(0.002));
        assert!(found(find(&db, "ML14219A568")).precursor.is_none());
    }

    #[test]
    fn precursor_list_includes_lers_not_in_db() {
        let db = fixture();
        let q = |min_ccdp, dockets: Option<Vec<String>>| {
            list_precursors(
                &db,
                &PrecursorQuery {
                    dockets,
                    min_ccdp,
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let all = q(None, None);
        assert_eq!(all.total, 2);
        assert_eq!(all.results[0].ler, "05000327/1975-006", "riskiest first");
        assert_eq!(all.results[0].plant.as_deref(), Some("Sequoyah Unit 1"));
        assert_eq!(all.results[0].accession_number, None, "LER not in db");
        assert_eq!(
            all.results[1].accession_number.as_deref(),
            Some("ML14300A002")
        );
        assert_eq!(q(Some(0.01), None).total, 1);
        assert_eq!(q(None, Some(vec!["05000328".into()])).total, 1);
    }

    #[test]
    fn severity_filters_sort_and_score() {
        let db = fixture();
        // ML14219A568: containment degraded (3) + release_control lost (2) + cites
        // safety_function_failure (3) = 8.
        let any = Filters {
            any_barrier: terms(&["degraded", "breached"]),
            ..Default::default()
        };
        assert_eq!(search_sorted(&db, any, Sort::Newest), ["ML14219A568"]);
        let rcs = Filters {
            rcs_pressure_boundary: terms(&["breached"]),
            ..Default::default()
        };
        assert!(search_sorted(&db, rcs, Sort::Newest).is_empty());
        let lost = Filters {
            safety_functions_lost: terms(&["release_control"]),
            ..Default::default()
        };
        assert_eq!(search_sorted(&db, lost, Sort::Newest), ["ML14219A568"]);
        // (v) must not match (vii), which only the (hidden) rev 0 cites.
        let ssff = Filters {
            reported_under: terms(&["safety_function_failure"]),
            ..Default::default()
        };
        assert_eq!(search_sorted(&db, ssff, Sort::Newest), ["ML14219A568"]);
        let cc = Filters {
            reported_under: terms(&["common_cause"]),
            ..Default::default()
        };
        assert!(search_sorted(&db, cc, Sort::Newest).is_empty());
        let hits = search_events(
            &db,
            &Search {
                sort: Sort::Severity,
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap()
        .results;
        assert_eq!(hits[0].accession_number, "ML14219A568", "scored first");
        assert_eq!(hits[0].severity, Some(8));
        assert_eq!(
            hits[0].reported_under,
            [Criterion::SafetyFunctionFailure, Criterion::TsProhibited]
        );
        assert_eq!(hits[0].barriers, ["containment: degraded"]);
        assert_eq!(
            hits[0].safety_functions_lost,
            [SafetyFunction::ReleaseControl]
        );
        assert_eq!(hits[1].severity, None, "unclassified last");
    }

    #[test]
    fn environmental_release_filters() {
        let db = fixture();
        let env = || Filters {
            environmental_release: true,
            ..Default::default()
        };
        // The fixture event has radiological_release "none" and no non-radiological tags.
        assert!(search_sorted(&db, env(), Sort::Newest).is_empty());

        db.execute_batch(
            "INSERT INTO class_tags VALUES ('ML14219A568', 'non_radiological', 'oil_chemical_spill')",
        )
        .unwrap();
        assert_eq!(search_sorted(&db, env(), Sort::Newest), ["ML14219A568"]);
        let oil = Filters {
            non_radiological_release: terms(&["oil_chemical_spill"]),
            ..Default::default()
        };
        assert_eq!(search_sorted(&db, oil, Sort::Newest), ["ML14219A568"]);
        let thermal = Filters {
            non_radiological_release: terms(&["thermal_discharge"]),
            ..Default::default()
        };
        assert!(search_sorted(&db, thermal, Sort::Newest).is_empty());

        // A radiological release alone also counts; "unknown" does not.
        db.execute_batch(
            "DELETE FROM class_tags WHERE field = 'non_radiological';
             UPDATE classification SET radiological_release = 'unknown';",
        )
        .unwrap();
        assert!(search_sorted(&db, env(), Sort::Newest).is_empty());
        db.execute_batch("UPDATE classification SET radiological_release = 'unmonitored_path'")
            .unwrap();
        assert_eq!(search_sorted(&db, env(), Sort::Newest), ["ML14219A568"]);
    }

    #[test]
    fn summarize_by_severity_fields() {
        let db = fixture();
        let by = |g| {
            summarize_events(
                &db,
                &Summarize {
                    filters: Filters::default(),
                    group_by: g,
                    then_by: None,
                    top: 10,
                },
            )
            .unwrap()
        };
        let c = by(GroupBy::Containment);
        assert_eq!(c.groups[0].value, None, "one unclassified event");
        assert!(
            c.groups
                .iter()
                .any(|g| g.value.as_deref() == Some("degraded"))
        );
        let sf = by(GroupBy::SafetyFunctionsLost);
        assert!(sf.multi_valued);
        assert!(
            sf.groups
                .iter()
                .any(|g| g.value.as_deref() == Some("release_control"))
        );
    }

    fn search_sorted(db: &Connection, filters: Filters, sort: Sort) -> Vec<String> {
        search_events(
            db,
            &Search {
                filters,
                sort,
                limit: 10,
            },
        )
        .unwrap()
        .results
        .into_iter()
        .map(|r| r.accession_number)
        .collect()
    }

    #[test]
    fn search_latest_revisions_newest_first() {
        let db = fixture();
        assert_eq!(
            search(&db, Filters::default()),
            ["ML14219A568", "ML14300A002"],
            "rev 0 of U2 excluded"
        );
        assert_eq!(
            search_sorted(&db, Filters::default(), Sort::Oldest),
            ["ML14300A002", "ML14219A568"]
        );
    }

    #[test]
    fn search_by_system_code_or_name() {
        let db = fixture();
        for sys in ["nh", "Containment", "100%_x", "BA"] {
            let got = search(
                &db,
                Filters {
                    systems: vec![sys.into()],
                    ..Default::default()
                },
            );
            let want: &[&str] = if sys.eq_ignore_ascii_case("nh") || sys == "Containment" {
                &["ML14219A568"]
            } else {
                &[]
            };
            assert_eq!(got, want, "{sys}");
        }
        db.execute(
            "UPDATE classification SET system_name = 'balance of plant'",
            [],
        )
        .unwrap();
        let code_only = Filters {
            systems: vec!["ba".into()],
            ..Default::default()
        };
        assert!(search(&db, code_only).is_empty(), "codes aren't substrings");
        let any = Filters {
            systems: vec!["BA".into(), "balance".into()],
            ..Default::default()
        };
        assert_eq!(search(&db, any), ["ML14219A568"]);
    }

    #[test]
    fn search_text_plant_and_dates() {
        let db = fixture();
        let afw = |text: &str| Filters {
            text: Some(text.into()),
            ..Default::default()
        };
        assert_eq!(search(&db, afw("feedwater")), ["ML14300A002"]);
        assert_eq!(
            search(&db, afw("turbine-driven")),
            ["ML14300A002"],
            "hyphen retried quoted"
        );
        assert!(search(&db, afw("\"unbalanced")).is_empty());
        let plant = Filters {
            dockets: Some(vec!["05000327".into()]),
            ..Default::default()
        };
        assert_eq!(search(&db, plant), ["ML14219A568"]);
        let june = Filters {
            date_from: Some("2014-06".into()),
            date_to: Some("2014-06".into()),
            ..Default::default()
        };
        assert_eq!(search(&db, june), ["ML14219A568"]);
        let year = Filters {
            date_to: Some("2014".into()),
            ..Default::default()
        };
        assert_eq!(search(&db, year).len(), 2, "date_to year is inclusive");
    }

    #[test]
    fn search_by_classification_and_tags() {
        let db = fixture();
        let only = |s: Filters| search(&db, s);
        let hit = ["ML14219A568"];
        let none: [&str; 0] = [];
        let cases = [
            (
                Filters {
                    causes: terms(&["procedure", "design"]),
                    ..Default::default()
                },
                &hit[..],
            ),
            (
                Filters {
                    causes: terms(&["equipment"]),
                    ..Default::default()
                },
                &none[..],
            ),
            (
                Filters {
                    human_error: terms(&["contributing"]),
                    ..Default::default()
                },
                &hit[..],
            ),
            (
                Filters {
                    who: terms(&["licensed_operator", "engineering"]),
                    ..Default::default()
                },
                &hit[..],
            ),
            (
                Filters {
                    who: terms(&["licensed_operator"]),
                    ..Default::default()
                },
                &none[..],
            ),
            (
                Filters {
                    operator_fundamentals: terms(&["knowledge"]),
                    ..Default::default()
                },
                &none[..],
            ),
            (
                Filters {
                    who: terms(&["engineering"]),
                    human_error: terms(&["no"]),
                    ..Default::default()
                },
                &none[..],
            ),
        ];
        for (i, (s, want)) in cases.into_iter().enumerate() {
            assert_eq!(only(s), want, "case {i}");
        }
        let r = search_events(
            &db,
            &Search {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            r.results[0].who,
            [Who::Maintenance, Who::Engineering],
            "tags in results"
        );
        assert_eq!(r.results[0].human_error, Some(HumanError::Contributing));
    }

    fn summarize(db: &Connection, filters: Filters, group_by: GroupBy, top: usize) -> Breakdown {
        summarize_events(
            db,
            &Summarize {
                filters,
                group_by,
                then_by: None,
                top,
            },
        )
        .unwrap()
    }

    fn groups(b: &Breakdown) -> Vec<(Option<&str>, usize, f64)> {
        b.groups
            .iter()
            .map(|g| (g.value.as_deref(), g.events, g.pct))
            .collect()
    }

    #[test]
    fn summarize_single_valued_counts_unclassified_as_none() {
        let b = summarize(&fixture(), Filters::default(), GroupBy::Cause, 10);
        assert_eq!(b.total_events, 2);
        assert!(!b.multi_valued);
        assert_eq!(groups(&b), [(None, 1, 50.0), (Some("procedure"), 1, 50.0)]);
        assert_eq!((b.other_groups, b.other_events), (0, 0));
    }

    #[test]
    fn summarize_multi_valued_counts_each_event_once_per_value() {
        let b = summarize(&fixture(), Filters::default(), GroupBy::Who, 10);
        assert!(b.multi_valued);
        assert_eq!(
            groups(&b),
            [
                (None, 1, 50.0),
                (Some("engineering"), 1, 50.0),
                (Some("maintenance"), 1, 50.0)
            ]
        );
    }

    #[test]
    fn summarize_applies_filters_and_top() {
        let db = fixture();
        let unit1 = Filters {
            dockets: Some(vec!["05000327".into()]),
            ..Default::default()
        };
        let b = summarize(&db, unit1, GroupBy::Who, 1);
        assert_eq!(b.total_events, 1);
        assert_eq!(groups(&b), [(Some("engineering"), 1, 100.0)]);
        assert_eq!((b.other_groups, b.other_events), (1, 1));
        let text = Filters {
            text: Some("turbine-driven".into()),
            ..Default::default()
        };
        assert_eq!(
            summarize(&db, text, GroupBy::Cause, 10).total_events,
            1,
            "FTS retry"
        );
    }

    #[test]
    fn summarize_time_groups_in_order() {
        let db = fixture();
        db.execute(
            "UPDATE lers SET event_date = '2009-01-01' WHERE accession_number = 'ML14300A002'",
            [],
        )
        .unwrap();
        let b = summarize(&db, Filters::default(), GroupBy::Decade, 10);
        assert_eq!(
            groups(&b),
            [(Some("2000s"), 1, 50.0), (Some("2010s"), 1, 50.0)]
        );
        let y = summarize(&db, Filters::default(), GroupBy::Year, 10);
        assert_eq!(
            groups(&y),
            [(Some("2009"), 1, 50.0), (Some("2014"), 1, 50.0)]
        );
    }

    #[test]
    fn summarize_then_by_shares_of_each_group() {
        let db = fixture();
        let s = |g, t| Summarize {
            filters: Filters::default(),
            group_by: g,
            then_by: Some(t),
            top: 10,
        };
        let b = summarize_events(&db, &s(GroupBy::Decade, GroupBy::Cause)).unwrap();
        assert_eq!(b.then_by.as_deref(), Some("cause"));
        let g = &b.groups[0];
        assert_eq!((g.value.as_deref(), g.events), (Some("2010s"), 2));
        let sub: Vec<_> =
            g.by.iter()
                .map(|c| (c.value.as_deref(), c.events, c.pct))
                .collect();
        assert_eq!(sub, [(None, 1, 50.0), (Some("procedure"), 1, 50.0)]);

        // both multi-valued: each needs its own tag join
        db.execute(
            "INSERT INTO class_tags VALUES ('ML14219A568', 'operator_fundamentals', 'knowledge')",
            [],
        )
        .unwrap();
        let b = summarize_events(&db, &s(GroupBy::Who, GroupBy::OperatorFundamentals)).unwrap();
        assert!(b.multi_valued);
        let eng = b
            .groups
            .iter()
            .find(|g| g.value.as_deref() == Some("engineering"))
            .unwrap();
        assert_eq!(eng.by[0].value.as_deref(), Some("knowledge"));
        assert_eq!(eng.by[0].pct, 100.0);
    }

    #[test]
    fn search_total_counts_past_limit() {
        let r = search_events(
            &fixture(),
            &Search {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((r.total, r.results.len()), (2, 1));
    }

    fn text(db: &Connection, id: &str, sections: &[Section], max_chars: usize) -> EventText {
        let req = TextRequest {
            sections: sections.to_vec(),
            max_chars,
        };
        found(find_text(db, &id.parse().unwrap(), &req).unwrap())
    }

    fn section_names(t: &EventText) -> Vec<&str> {
        t.sections.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn text_default_sections_and_missing() {
        let t = text(&fixture(), "327-2014-002", Section::DEFAULT, 1000);
        assert_eq!(section_names(&t), ["abstract", "description", "cause"]);
        assert_eq!(t.missing, ["corrective"]);
        assert_eq!(t.title.as_deref(), Some("Clean U1 title"));
        assert!(t.sections.iter().all(|s| !s.truncated));
    }

    #[test]
    fn text_falls_back_to_narrative() {
        let t = text(&fixture(), "328-2014-002", Section::DEFAULT, 1000);
        assert_eq!(section_names(&t), ["narrative"]);
        assert_eq!(t.sections[0].text, "Only an unsplit narrative here.");
    }

    #[test]
    fn text_budget_truncates_later_sections() {
        let t = text(
            &fixture(),
            "ML14219A568",
            &[Section::Abstract, Section::Description],
            30,
        );
        assert_eq!(t.sections[0].text, "Penetrations were opened.");
        let d = &t.sections[1];
        assert_eq!((d.text.chars().count(), d.truncated), (5, true));
        assert_eq!(
            d.chars,
            "The penetrations were opened during fuel movement.".len()
        );
    }

    #[test]
    fn text_ambiguous_like_get_event() {
        let req = TextRequest {
            sections: Section::DEFAULT.to_vec(),
            max_chars: 100,
        };
        let l = find_text(&fixture(), &"2014-002".parse().unwrap(), &req).unwrap();
        assert!(matches!(l, Lookup::Ambiguous(c) if c.total == 2));
    }

    #[test]
    fn not_found() {
        let db = fixture();
        for id in ["ML99999A999", "327-2015-001", "2014-003", "327-2014-002-05"] {
            assert_eq!(find(&db, id), Lookup::NotFound, "{id}");
        }
    }
}
