//! ler-mcp: an MCP server that lets an AI assistant query lers_analysis.db.
//!
//! How MCP works here: the AI client (Claude Code, Claude Desktop, ...) starts this program as a
//! child process and talks JSON-RPC over stdin/stdout. It asks "what tools do you have?"
//! (tools/list) and we answer with each tool's name, description, and JSON Schemas for its
//! input and output. When the model uses a tool, the client sends tools/call and we reply with
//! text for the model plus the same data as structured JSON.
//! stdout IS the protocol channel: never println!. Logs go to stderr via `tracing`.
//!
//! Layout: ids.rs parses user input, vocab.rs holds the filter vocabularies, db.rs runs
//! queries, report.rs holds the output types.

mod db;
#[cfg(test)]
mod evals;
mod ids;
mod plants;
mod report;
mod vocab;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{common::schema_for_output, router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService, stdio,
        streamable_http_server::session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::db::{
    Db, Filters, GroupBy, Lookup, PrecursorQuery, Search, Section, Sort, Summarize, TextRequest,
};
use crate::ids::EventId;
use crate::report::{Breakdown, EventReport, EventText, PrecursorList, SearchResults};
use crate::vocab::{
    BarrierState, Cause, Criterion, HumanError, NonRadiologicalRelease, OperatorFundamental,
    RadiologicalRelease, ReactorType, SafetyFunction, Who,
};

const INSTRUCTIONS: &str = "\
Licensee Event Reports (LERs) filed with the US NRC by commercial nuclear power plants, \
from the NRC ADAMS library. Each event has an AI classification (cause, human performance, \
system and failure mode, fission product barriers, environmental release, generation impact) \
made by Claude Haiku from the report text (releases to the environment rechecked in a second, \
narrower pass); treat it as a first pass, and quote the report \
itself (PDF link) for anything that matters. Only the latest revision of each LER is classified.

Keep sources apart in answers: say what comes from this server (cite the LER number or \
accession number) and what comes from your own knowledge or anywhere else, and never present \
outside knowledge as if it were in this database. When something isn't found here, say it isn't \
in this database, not that it didn't happen.";

/// Server state. rmcp clones the handler per request, so everything here is cheap to clone
/// (`Db` is an Arc'd connection pool).
#[derive(Clone)]
struct LerServer {
    db: Db,
    #[allow(dead_code)] // read by the code #[tool_handler] generates
    tool_router: ToolRouter<Self>,
}

/// Arguments for get_event, exactly as the client sends them. Validation happens in
/// `EventId::from_str`, so this stays a plain wire type. Doc comments become the schema
/// descriptions the model reads.
#[derive(Deserialize, JsonSchema)]
struct GetEventArgs {
    /// An LER number with docket ("327-2014-002", "05000327/2014-002-01", "50-327-2014-002"),
    /// an LER number without docket ("2014-002", matches every plant that used that number), or an ADAMS
    /// accession number ("ML14219A568"). Without a revision, the latest revision is returned.
    id: String,
}

/// Arguments for get_event_text.
#[derive(Deserialize, JsonSchema)]
struct GetEventTextArgs {
    /// Same as get_event: an LER number (with or without docket) or an ADAMS accession number.
    id: String,
    /// Sections to return, in order. Default: abstract, description, cause, corrective. Others:
    /// condition_prior (plant conditions before the event), safety (safety assessment),
    /// previous (previous similar events), narrative (the whole text, often long). When none of
    /// the requested sections were found, the whole narrative is returned instead.
    #[serde(default)]
    sections: Vec<SectionArg>,
    /// Total characters to return, 500 to 40000; default 12000. Sections are cut to fit, in order.
    max_chars: Option<usize>,
}

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum SectionArg {
    Abstract,
    ConditionPrior,
    Description,
    Cause,
    Safety,
    Corrective,
    Previous,
    Narrative,
}

impl From<SectionArg> for Section {
    fn from(s: SectionArg) -> Self {
        match s {
            SectionArg::Abstract => Section::Abstract,
            SectionArg::ConditionPrior => Section::ConditionPrior,
            SectionArg::Description => Section::Description,
            SectionArg::Cause => Section::Cause,
            SectionArg::Safety => Section::Safety,
            SectionArg::Corrective => Section::Corrective,
            SectionArg::Previous => Section::Previous,
            SectionArg::Narrative => Section::Narrative,
        }
    }
}

const DEFAULT_TEXT_CHARS: usize = 12_000;
const TEXT_CHARS: std::ops::RangeInclusive<usize> = 500..=40_000;

impl From<&GetEventTextArgs> for TextRequest {
    fn from(a: &GetEventTextArgs) -> Self {
        let mut sections: Vec<Section> = vec![];
        for s in a.sections.iter().map(|&s| Section::from(s)) {
            if !sections.contains(&s) {
                sections.push(s);
            }
        }
        if sections.is_empty() {
            sections = Section::DEFAULT.to_vec();
        }
        TextRequest {
            sections,
            max_chars: a
                .max_chars
                .unwrap_or(DEFAULT_TEXT_CHARS)
                .clamp(*TEXT_CHARS.start(), *TEXT_CHARS.end()),
        }
    }
}

/// Filters shared by search_events and summarize_events. All are optional and combine with AND;
/// a list matches if the event has any of its values.
#[derive(Deserialize, JsonSchema)]
struct FilterArgs {
    /// Systems to match; an event matches if any one does. A two-letter entry is an EIIS
    /// system code, compared exactly ("BA" = auxiliary/emergency feedwater); anything longer is a
    /// case-insensitive substring of the classified system name ("auxiliary feedwater").
    /// Plants name systems differently, so list synonyms, e.g. ["BA", "auxiliary feedwater",
    /// "emergency feedwater"]. Prefer this over `text` for "events involving system X":
    /// text search also finds reports that only mention the system in passing.
    #[serde(default)]
    system: Vec<String>,
    /// Full-text search of title, abstract and narrative (SQLite FTS5, English stemming):
    /// words are ANDed; use "quoted phrases", OR, NOT, and prefix* as needed.
    text: Option<String>,
    /// Plant as people say it: a name ("Sequoyah", "St. Lucie"), an abbreviation ("SQN",
    /// "PVNGS"), a unit ("Palo Verde Unit 2", "Fermi 2"), or a docket ("327", "05000327").
    /// Misspellings are matched to the closest plant and noted in the result.
    plant: Option<String>,
    /// Earliest event date, inclusive: "2020", "2020-06" or "2020-06-15".
    date_from: Option<String>,
    /// Latest event date, inclusive: "2020" includes all of 2020.
    date_to: Option<String>,
    /// Reactor type. ["PWR", "BWR"] = commercial power reactors, the only ones classified; use
    /// it for statistics about power plants.
    #[serde(default)]
    reactor_type: Vec<ReactorType>,
    /// Primary cause.
    #[serde(default)]
    cause: Vec<Cause>,
    /// Whether human error was involved. ["yes", "contributing"] finds every event with human
    /// error in it.
    #[serde(default)]
    human_error: Vec<HumanError>,
    /// Who made the errors. "Operator events" usually means licensed_operator (control room ROs
    /// and SROs), often with non_licensed_operator (equipment operators in the plant).
    #[serde(default)]
    who: Vec<Who>,
    /// INPO operator fundamentals weak in the event.
    #[serde(default)]
    operator_fundamentals: Vec<OperatorFundamental>,
    /// Fuel cladding condition: none, potential, challenged, degraded, breached, least to most
    /// severe. The same scale applies to rcs_pressure_boundary and containment.
    #[serde(default)]
    fuel_cladding: Vec<BarrierState>,
    /// Reactor coolant system pressure boundary condition (same scale as fuel_cladding);
    /// "breached" = actual pressure boundary leakage, such as a through-wall crack.
    #[serde(default)]
    rcs_pressure_boundary: Vec<BarrierState>,
    /// Containment condition (same scale as fuel_cladding).
    #[serde(default)]
    containment: Vec<BarrierState>,
    /// Matches if any of the three barriers is in one of these states, e.g. ["breached"] or
    /// ["degraded", "breached"] for serious events.
    #[serde(default)]
    any_barrier: Vec<BarrierState>,
    /// Radioactive material the event put into the environment. A degraded or breached barrier
    /// is not a release by itself: use the barrier filters for that.
    #[serde(default)]
    radiological_release: Vec<RadiologicalRelease>,
    /// Non-radioactive releases to the environment: oil or chemical spills, thermal discharges,
    /// permit exceedances.
    #[serde(default)]
    non_radiological_release: Vec<NonRadiologicalRelease>,
    /// true = only events that put anything into the environment: a radiological release
    /// (within limits, above limits, or by an unmonitored path) or any non-radiological one.
    /// Use this for "releases to the environment" questions.
    #[serde(default)]
    environmental_release: bool,
    /// Safety functions the LER says were (or could have been) lost.
    #[serde(default)]
    safety_functions_lost: Vec<SafetyFunction>,
    /// Reporting criteria the licensee cited: their own call, steadier than the AI barrier
    /// ratings.
    #[serde(default)]
    reported_under: Vec<Criterion>,
    /// true = only events the NRC's Accident Sequence Precursor (ASP) program rated as
    /// precursors: its own risk estimate, the best measure of how close an event came to core
    /// damage. Covers mainly 1990s-present, plus every significant precursor since 1969 that is
    /// in this database.
    #[serde(default)]
    precursor: bool,
    /// Only precursors whose CCDP/delta-CDP (the NRC's estimated chance the event would have led
    /// to core damage) is at least this, e.g. 0.001 for "significant precursors" (1 in 1,000).
    min_ccdp: Option<f64>,
}

/// Arguments for search_events.
#[derive(Deserialize, JsonSchema)]
struct SearchEventsArgs {
    #[serde(flatten)]
    filters: FilterArgs,
    /// "risk": the NRC's precursor estimate (CCDP/delta-CDP), highest first; use it for "closest
    /// to disaster" or "most risk-significant" questions, as it is the NRC's own number, not
    /// the AI's. Events the NRC never rated come last.
    /// "newest" (default), "oldest", "relevance" (needs `text`), or "severity": a rough score,
    /// highest first. AI classification part: fuel cladding 0-4 counted double, RCS pressure
    /// boundary and containment 0-4 each (none 0, potential 1, challenged 2, degraded 3,
    /// breached 4), a release (within limits 2, unmonitored path 4, above limits 6), 2 per
    /// safety function lost. Licensee part (reported_under): emergency_declared 6,
    /// seriously_degraded 6, safety_function_failure 3, actual_threat 3, unanalyzed_condition 2,
    /// external_threat 2. For "worst" or "most serious" questions, combine it with
    /// reported_under ["seriously_degraded", "emergency_declared"] and read the top reports:
    /// the score is a screen, not a risk measure.
    sort: Option<SortArg>,
    /// Number of results, 1 to 50; default 10.
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum SortArg {
    Newest,
    Oldest,
    Relevance,
    Severity,
    Risk,
}

/// Arguments for list_precursors.
#[derive(Deserialize, JsonSchema)]
struct ListPrecursorsArgs {
    /// Plant, as for search_events ("Browns Ferry", "SQN", "Davis-Besse", a docket).
    plant: Option<String>,
    /// Earliest event date, inclusive: "1990", "2002-02".
    date_from: Option<String>,
    /// Latest event date, inclusive.
    date_to: Option<String>,
    /// Only precursors with CCDP/delta-CDP at least this; 0.001 = significant precursors.
    min_ccdp: Option<f64>,
    /// Number of results, 1 to 50; default 10.
    limit: Option<usize>,
}

fn check_ccdp(v: Option<f64>) -> Result<Option<f64>, String> {
    match v {
        Some(v) if !(v > 0.0 && v <= 1.0) => Err(format!(
            "min_ccdp must be a probability above 0 and at most 1, got {v}"
        )),
        v => Ok(v),
    }
}

impl TryFrom<ListPrecursorsArgs> for PrecursorQuery {
    type Error = String;

    fn try_from(a: ListPrecursorsArgs) -> Result<Self, String> {
        Ok(PrecursorQuery {
            dockets: None,
            date_from: check_date("date_from", a.date_from)?,
            date_to: check_date("date_to", a.date_to)?,
            min_ccdp: check_ccdp(a.min_ccdp)?,
            limit: a.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        })
    }
}

/// Arguments for summarize_events.
#[derive(Deserialize, JsonSchema)]
struct SummarizeEventsArgs {
    #[serde(flatten)]
    filters: FilterArgs,
    /// Field to count events by. who, operator_fundamentals, safety_functions_lost and
    /// non_radiological_release are multi-valued: an event counts once under each value. system = EIIS system code. year/decade use the event date (report
    /// date when missing) and come back in time order; the rest by count.
    group_by: GroupByArg,
    /// Optional second field, for cross-tabs like cause by decade (group_by "decade", then_by
    /// "cause"): each group is broken down by it, with percentages of that group. Same choices
    /// as group_by.
    then_by: Option<GroupByArg>,
    /// Most groups to return at each level, 1 to 100; default 25. The rest are totalled.
    top: Option<usize>,
}

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum GroupByArg {
    Cause,
    HumanError,
    ErrorType,
    Who,
    OperatorFundamentals,
    FailureMode,
    FailureMechanism,
    System,
    Plant,
    ReactorType,
    Year,
    Decade,
    FuelCladding,
    RcsPressureBoundary,
    Containment,
    RadiologicalRelease,
    NonRadiologicalRelease,
    SafetyFunctionsLost,
}

impl From<GroupByArg> for GroupBy {
    fn from(g: GroupByArg) -> Self {
        match g {
            GroupByArg::Cause => GroupBy::Cause,
            GroupByArg::HumanError => GroupBy::HumanError,
            GroupByArg::ErrorType => GroupBy::ErrorType,
            GroupByArg::Who => GroupBy::Who,
            GroupByArg::OperatorFundamentals => GroupBy::OperatorFundamentals,
            GroupByArg::FailureMode => GroupBy::FailureMode,
            GroupByArg::FailureMechanism => GroupBy::FailureMechanism,
            GroupByArg::System => GroupBy::System,
            GroupByArg::Plant => GroupBy::Plant,
            GroupByArg::ReactorType => GroupBy::ReactorType,
            GroupByArg::Year => GroupBy::Year,
            GroupByArg::Decade => GroupBy::Decade,
            GroupByArg::FuelCladding => GroupBy::FuelCladding,
            GroupByArg::RcsPressureBoundary => GroupBy::RcsPressureBoundary,
            GroupByArg::Containment => GroupBy::Containment,
            GroupByArg::RadiologicalRelease => GroupBy::RadiologicalRelease,
            GroupByArg::NonRadiologicalRelease => GroupBy::NonRadiologicalRelease,
            GroupByArg::SafetyFunctionsLost => GroupBy::SafetyFunctionsLost,
        }
    }
}

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
const DEFAULT_TOP: usize = 25;
const MAX_TOP: usize = 100;

/// "YYYY", "YYYY-MM" or "YYYY-MM-DD"; these compare correctly as strings against ISO dates.
fn check_date(field: &str, v: Option<String>) -> Result<Option<String>, String> {
    let Some(v) = v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let ok = matches!(v.len(), 4 | 7 | 10)
        && v.char_indices().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        });
    if ok {
        Ok(Some(v))
    } else {
        Err(format!(
            "{field} must be YYYY, YYYY-MM or YYYY-MM-DD, got {v:?}"
        ))
    }
}

impl TryFrom<FilterArgs> for Filters {
    type Error = String;

    fn try_from(a: FilterArgs) -> Result<Self, String> {
        let nonblank =
            |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Ok(Filters {
            systems: a
                .system
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            text: nonblank(a.text),
            plant: nonblank(a.plant),
            dockets: None,
            date_from: check_date("date_from", a.date_from)?,
            date_to: check_date("date_to", a.date_to)?,
            reactor_types: a.reactor_type,
            causes: a.cause,
            human_error: a.human_error,
            who: a.who,
            operator_fundamentals: a.operator_fundamentals,
            fuel_cladding: a.fuel_cladding,
            rcs_pressure_boundary: a.rcs_pressure_boundary,
            containment: a.containment,
            any_barrier: a.any_barrier,
            radiological_release: a.radiological_release,
            non_radiological_release: a.non_radiological_release,
            environmental_release: a.environmental_release,
            safety_functions_lost: a.safety_functions_lost,
            reported_under: a.reported_under,
            precursor: a.precursor,
            min_ccdp: check_ccdp(a.min_ccdp)?,
        })
    }
}

impl TryFrom<SearchEventsArgs> for Search {
    type Error = String;

    fn try_from(a: SearchEventsArgs) -> Result<Self, String> {
        let filters = Filters::try_from(a.filters)?;
        let sort = match a.sort.unwrap_or(SortArg::Newest) {
            SortArg::Newest => Sort::Newest,
            SortArg::Oldest => Sort::Oldest,
            SortArg::Relevance if filters.text.is_none() => {
                return Err("sort \"relevance\" needs a text query".into());
            }
            SortArg::Relevance => Sort::Relevance,
            SortArg::Severity => Sort::Severity,
            SortArg::Risk => Sort::Risk,
        };
        Ok(Search {
            filters,
            sort,
            limit: a.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        })
    }
}

impl TryFrom<SummarizeEventsArgs> for Summarize {
    type Error = String;

    fn try_from(a: SummarizeEventsArgs) -> Result<Self, String> {
        let group_by: GroupBy = a.group_by.into();
        let then_by: Option<GroupBy> = a.then_by.map(Into::into);
        if then_by == Some(group_by) {
            return Err("then_by must differ from group_by".into());
        }
        Ok(Summarize {
            filters: Filters::try_from(a.filters)?,
            group_by,
            then_by,
            top: a.top.unwrap_or(DEFAULT_TOP).clamp(1, MAX_TOP),
        })
    }
}

impl LerServer {
    fn new(db: Db) -> Self {
        Self {
            db,
            tool_router: Self::tool_router(),
        }
    }

    /// Turns the typed plant into dockets; returns the note saying how it was read.
    fn resolve_plant(&self, filters: &mut Filters) -> Result<Vec<String>, String> {
        let Some(p) = &filters.plant else {
            return Ok(vec![]);
        };
        let m = self
            .db
            .plants()
            .resolve(p)
            .inspect_err(|e| warn!(error = %e, "plant"))?;
        filters.dockets = Some(m.dockets);
        Ok(vec![m.note])
    }
}

#[tool_router]
impl LerServer {
    #[tool(
        description = "Get one Licensee Event Report (LER) by LER number or ADAMS accession number. \
            Returns title, site, unit, dates, PDF link, and the AI classification (cause, system, \
            barriers, generation impact per unit). LER numbers restart per docket each year, so a number \
            without a docket usually matches several plants; then a list to choose from is returned.",
        output_schema = schema_for_output::<EventReport>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(skip_all, fields(id = %args.id))]
    async fn get_event(
        &self,
        Parameters(args): Parameters<GetEventArgs>,
    ) -> Result<CallToolResult, String> {
        let id: EventId = args.id.parse().map_err(|e| {
            warn!(error = %e, "bad id");
            format!("{e}")
        })?;
        let lookup = self.db.find_event(id).await.map_err(|e| {
            warn!(error = %e, "lookup failed");
            e.to_string()
        })?;
        match lookup {
            Lookup::Found(report) => {
                info!(accession = %report.accession_number, "found");
                Ok(report_result(&report))
            }
            Lookup::Ambiguous(candidates) => {
                info!(matches = candidates.total, "ambiguous");
                Err(candidates.to_string())
            }
            Lookup::NotFound => {
                info!("not found");
                Err(format!("no LER found for {:?}", args.id))
            }
        }
    }

    #[tool(
        description = "Get the text of one LER, as the licensee wrote it: abstract and narrative \
            sections (description, cause, corrective actions, ...). Use it to quote the report or check \
            the AI classification; get_event gives the classification and metadata. Same ids as \
            get_event. Text is OCR of the filed report.",
        output_schema = schema_for_output::<EventText>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(skip_all, fields(id = %args.id))]
    async fn get_event_text(
        &self,
        Parameters(args): Parameters<GetEventTextArgs>,
    ) -> Result<CallToolResult, String> {
        let id: EventId = args.id.parse().map_err(|e| {
            warn!(error = %e, "bad id");
            format!("{e}")
        })?;
        let req = TextRequest::from(&args);
        let lookup = self.db.find_text(id, req).await.map_err(|e| {
            warn!(error = %e, "text lookup failed");
            e.to_string()
        })?;
        match lookup {
            Lookup::Found(text) => {
                info!(accession = %text.accession_number, sections = text.sections.len(), "text");
                let mut result =
                    CallToolResult::success(vec![ContentBlock::text(text.to_string())]);
                result.structured_content = serde_json::to_value(&*text).ok();
                Ok(result)
            }
            Lookup::Ambiguous(candidates) => Err(candidates.to_string()),
            Lookup::NotFound => Err(format!("no LER found for {:?}", args.id)),
        }
    }

    #[tool(
        description = "Search Licensee Event Reports (latest revision of each) by system, full text, \
            plant, event date, cause, human performance (who erred, operator fundamentals), and \
            severity (barrier condition, radiological release, safety functions lost), and releases to \
            the environment (environmental_release true; radiological or non-radiological such as oil \
            spills). For \"worst\" or \
            \"most serious\" events use sort \"severity\"; for \"closest to disaster\" or \"most \
            risk-significant\" use sort \"risk\" (the NRC's own precursor estimates). Returns short summaries, newest first by default: LER and accession \
            number, plant, date, title, classified system, cause, failure mode, human error and \
            who, and a one-paragraph summary. Use get_event for the full record. For \"the last event involving X\", filter by \
            `system` with limit 1. About a quarter of events lack a parsed event date; those sort and \
            filter by report date.",
        output_schema = schema_for_output::<SearchResults>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(skip_all)]
    async fn search_events(
        &self,
        Parameters(args): Parameters<SearchEventsArgs>,
    ) -> Result<CallToolResult, String> {
        let mut search = Search::try_from(args).inspect_err(|e| warn!(error = %e, "bad search"))?;
        let notes = self.resolve_plant(&mut search.filters)?;
        info!(?search, "search");
        let mut results = self.db.search_events(search).await.map_err(|e| {
            warn!(error = %e, "search failed");
            e.to_string()
        })?;
        results.notes = notes;
        info!(total = results.total, "searched");
        let mut result = CallToolResult::success(vec![ContentBlock::text(results.to_string())]);
        result.structured_content = serde_json::to_value(&results).ok();
        Ok(result)
    }

    #[tool(
        description = "List the NRC's Accident Sequence Precursor (ASP) results, riskiest first: \
            the NRC's own estimate of how close each event came to core damage (CCDP for an \
            initiating event, delta-CDP for a degraded condition; 1e-3 or more = significant \
            precursor). Use it for \"closest to disaster\", \"most dangerous\" or \"riskiest\" \
            questions. Covers every significant precursor since 1969 and the yearly results \
            since about 2000; includes precursors whose LER isn't in this database (most \
            pre-1999 LERs), linking to the LER when it is (accession number for get_event).",
        output_schema = schema_for_output::<PrecursorList>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(skip_all)]
    async fn list_precursors(
        &self,
        Parameters(args): Parameters<ListPrecursorsArgs>,
    ) -> Result<CallToolResult, String> {
        let plant = args.plant.clone().filter(|p| !p.trim().is_empty());
        let mut q =
            PrecursorQuery::try_from(args).inspect_err(|e| warn!(error = %e, "bad precursors"))?;
        let mut notes = vec![];
        if let Some(p) = plant {
            let m = self
                .db
                .plants()
                .resolve(&p)
                .inspect_err(|e| warn!(error = %e, "plant"))?;
            q.dockets = Some(m.dockets);
            notes.push(m.note);
        }
        info!(?q, "precursors");
        let mut list = self.db.list_precursors(q).await.map_err(|e| {
            warn!(error = %e, "precursors failed");
            e.to_string()
        })?;
        list.notes = notes;
        let mut result = CallToolResult::success(vec![ContentBlock::text(list.to_string())]);
        result.structured_content = serde_json::to_value(&list).ok();
        Ok(result)
    }

    #[tool(
        description = "Count events by one field, for statistics and \"what contributes most\" \
            questions: cause, human_error, error_type, who, operator_fundamentals, failure_mode, \
            failure_mechanism, system (EIIS code), plant, reactor_type, year or decade, the severity \
            fields (fuel_cladding, rcs_pressure_boundary, containment, radiological_release, \
            safety_functions_lost), non_radiological_release, optionally \
            broken down by a second field (then_by) for cross-tabs such as cause by decade. Takes the \
            same filters as search_events (e.g. reactor_type [\"PWR\",\"BWR\"], a plant, a date \
            range, cause [\"equipment\"] before grouping by failure_mechanism, human_error \
            [\"yes\",\"contributing\"] before grouping by who). Returns counts and percent of the \
            matching events. Counts come from the AI classification: a first pass, not the licensee's \
            own coding.",
        output_schema = schema_for_output::<Breakdown>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(skip_all)]
    async fn summarize_events(
        &self,
        Parameters(args): Parameters<SummarizeEventsArgs>,
    ) -> Result<CallToolResult, String> {
        let mut s =
            Summarize::try_from(args).inspect_err(|e| warn!(error = %e, "bad summarize"))?;
        let notes = self.resolve_plant(&mut s.filters)?;
        info!(?s, "summarize");
        let mut b = self.db.summarize_events(s).await.map_err(|e| {
            warn!(error = %e, "summarize failed");
            e.to_string()
        })?;
        b.notes = notes;
        info!(
            total = b.total_events,
            groups = b.groups.len(),
            "summarized"
        );
        let mut result = CallToolResult::success(vec![ContentBlock::text(b.to_string())]);
        result.structured_content = serde_json::to_value(&b).ok();
        Ok(result)
    }
}

/// Text for the model (`Display`) plus the same data as structured content (`Serialize`),
/// which must match the tool's declared output schema.
fn report_result(report: &EventReport) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(report.to_string())]);
    result.structured_content = serde_json::to_value(report).ok();
    result
}

#[tool_handler]
impl ServerHandler for LerServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!("{INSTRUCTIONS}\n\n{}", self.db.info()))
    }
}

/// Log to stderr, level from RUST_LOG (e.g. `RUST_LOG=debug`, `RUST_LOG=ler_mcp=debug,rmcp=info`).
/// Defaults to `info` when RUST_LOG is unset or invalid. rmcp also logs through `tracing`,
/// so `RUST_LOG=rmcp=debug` shows the protocol traffic.
/// No ANSI colors: MCP clients usually capture stderr into a log file.
fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

/// MCP server for querying NRC Licensee Event Reports in lers_analysis.db.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to lers_analysis.db. No default, because MCP clients don't promise a working directory.
    #[arg(env = "LER_DB")]
    db: PathBuf,

    /// Serve MCP over HTTP at this address (e.g. 127.0.0.1:8090) and keep running, instead of
    /// speaking MCP on stdin/stdout for a client that starts this process.
    #[arg(long, value_name = "ADDR")]
    http: Option<SocketAddr>,
}

/// stdio: the client started us; serve one session on stdin/stdout until it disconnects.
async fn serve_stdio(db: Db) -> anyhow::Result<()> {
    let service = LerServer::new(db).serve(stdio()).await?;
    let reason = service.waiting().await?;
    info!(?reason, "stopped");
    Ok(())
}

/// HTTP: long-running; each client connection gets its own session (and its own LerServer,
/// all sharing one connection pool). Endpoint is http://ADDR/mcp. rmcp's default config only
/// accepts loopback Host headers, which guards against DNS-rebinding from web pages.
async fn serve_http(db: Db, addr: SocketAddr) -> anyhow::Result<()> {
    let service: StreamableHttpService<LerServer, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(LerServer::new(db.clone())),
        Default::default(),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    info!(
        "listening on http://{}/mcp (Ctrl-C to stop)",
        listener.local_addr()?
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging();
    let db = Db::open(&cli.db).with_context(|| format!("opening {}", cli.db.display()))?;
    info!(db = %cli.db.display(), data = %db.info(), "starting");
    match cli.http {
        Some(addr) => serve_http(db, addr).await,
        None => serve_stdio(db).await,
    }
}
