//! Tool output types. Plain data: db.rs fills them, `Display` renders the text the model reads,
//! and `Serialize` + `JsonSchema` give the same data as MCP structured content.

use std::fmt;

use schemars::JsonSchema;
use serde::Serialize;

use crate::vocab::{
    BarrierState, Cause, Criterion, HumanError, NonRadiologicalRelease, OperatorFundamental,
    RadiologicalRelease, ReactorType, SafetyFunction, Who,
};

/// One LER document with its classification (if any).
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct EventReport {
    pub accession_number: String,
    /// "2014-002-00"; absent for documents whose LER number couldn't be parsed.
    pub ler_number: Option<String>,
    /// 8-digit reporting docket, e.g. "05000327".
    pub docket: Option<String>,
    pub title: Option<String>,
    pub site: Option<String>,
    pub reporting_unit: Option<String>,
    pub reactor_type: Option<ReactorType>,
    pub event_date: Option<String>,
    pub report_date: Option<String>,
    pub pdf_url: Option<String>,
    /// AI classification; absent when the document wasn't classified.
    pub classification: Option<Classification>,
    /// NRC Accident Sequence Precursor result; absent when the NRC didn't rate it a precursor.
    pub precursor: Option<Precursor>,
    /// Things the reader should know, e.g. a newer revision exists.
    pub notes: Vec<String>,
}

/// The NRC's own risk estimate for an event (Accident Sequence Precursor program): the
/// conditional core damage probability (CCDP) of an initiating event, or the increase in core
/// damage probability (delta-CDP) from a degraded condition. Precursors are 1e-6 or more;
/// "significant" precursors 1e-3 or more.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Precursor {
    /// CCDP or delta-CDP, the NRC's latest estimate; absent when only a finding color is given.
    pub ccdp: Option<f64>,
    /// Inspection finding color (White, Yellow, Red) given instead of a number.
    pub finding: Option<String>,
    /// ccdp >= 1e-3.
    pub significant: bool,
    /// NRC documents (yearly ASP results, analyses) that list it.
    pub sources: i64,
    /// Different estimates seen across those documents (revised over the years), if any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_estimates: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Classification {
    pub summary: Option<String>,
    pub primary_cause: Option<Cause>,
    pub human_error: Option<HumanError>,
    pub error_type: Option<String>,
    pub who: Vec<Who>,
    /// INPO operator fundamentals weak in the event.
    pub operator_fundamentals: Vec<OperatorFundamental>,
    pub system_name: Option<String>,
    pub eiis_code: Option<String>,
    pub component: Option<String>,
    pub failure_mode: Option<String>,
    pub failure_mechanism: Option<String>,
    pub barriers: Barriers,
    /// Radioactive material the event put into the environment.
    pub radiological_release: Option<RadiologicalRelease>,
    /// Where released radioactive material went, e.g. "condensate spill to yard and storm drain".
    pub release_pathway: Option<String>,
    /// Non-radioactive material or heat the event put into the environment.
    pub non_radiological_release: Vec<NonRadiologicalRelease>,
    /// What was released and where.
    pub non_radiological_description: Option<String>,
    pub trip_risk: Option<String>,
    pub generation: Vec<UnitImpact>,
    pub confidence: Option<String>,
    /// "p4 / claude-haiku-5-5": which prompt and model produced this.
    pub source: String,
}

/// Fission product barrier conditions.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Barriers {
    pub fuel_cladding: Option<BarrierState>,
    pub rcs_pressure_boundary: Option<BarrierState>,
    pub containment: Option<BarrierState>,
}

/// Generation impact on one unit at the site.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct UnitImpact {
    pub unit_name: String,
    pub docket: Option<String>,
    pub event_occurred_here: bool,
    pub impact: Option<String>,
    pub power_before_pct: Option<i64>,
    pub power_after_pct: Option<i64>,
    pub duration_hours: Option<f64>,
}

/// One of several LERs matching an ambiguous reference.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Candidate {
    pub accession_number: String,
    pub ler_number: Option<String>,
    pub docket: Option<String>,
    pub reporting_unit: Option<String>,
    pub title: Option<String>,
}

/// The candidates for an ambiguous id (possibly capped), rendered as a prompt to call again
/// with a specific id.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidates {
    pub shown: Vec<Candidate>,
    /// How many matched in total; more than `shown.len()` when the list was capped.
    pub total: usize,
}

/// One search hit: enough to pick an event, then `get_event` for the rest.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct EventSummary {
    pub accession_number: String,
    pub ler_number: Option<String>,
    pub docket: Option<String>,
    pub site: Option<String>,
    pub reporting_unit: Option<String>,
    /// Absent for about a quarter of events (not parsed from the form); then sorting uses report_date.
    pub event_date: Option<String>,
    pub report_date: Option<String>,
    pub title: Option<String>,
    /// AI classification fields; absent when the event wasn't classified.
    pub system_name: Option<String>,
    pub eiis_code: Option<String>,
    pub primary_cause: Option<Cause>,
    pub failure_mode: Option<String>,
    pub human_error: Option<HumanError>,
    /// Who made the errors.
    pub who: Vec<Who>,
    /// INPO operator fundamentals weak in the event.
    pub operator_fundamentals: Vec<OperatorFundamental>,
    pub summary: Option<String>,
    /// Fission product barriers not rated "none", e.g. "rcs_pressure_boundary: breached".
    pub barriers: Vec<String>,
    /// Radioactive material the event put into the environment; absent when none.
    pub radiological_release: Option<RadiologicalRelease>,
    /// Non-radioactive material or heat the event put into the environment.
    pub non_radiological_release: Vec<NonRadiologicalRelease>,
    /// Safety functions the LER says were (or could have been) lost.
    pub safety_functions_lost: Vec<SafetyFunction>,
    /// Reporting criteria the licensee cites.
    pub reported_under: Vec<Criterion>,
    /// NRC precursor (ASP) result, when the NRC rated the event a precursor.
    pub precursor: Option<Precursor>,
    /// Rough severity score (see sort "severity"); absent when unclassified.
    pub severity: Option<i64>,
}

/// Search hits, newest first unless relevance was asked for.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct SearchResults {
    /// How the filters were read, e.g. which plant a misspelled name was taken as.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// How many events matched in total; more than `results.len()` when limited.
    pub total: usize,
    pub results: Vec<EventSummary>,
}

/// Event counts by one field, for the events matching some filters.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Breakdown {
    /// How the filters were read, e.g. which plant a misspelled name was taken as.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The field counted by, e.g. "cause".
    pub group_by: String,
    /// The second field each group is broken down by, if any.
    pub then_by: Option<String>,
    /// True when either field is who / operator_fundamentals: one event can be in several
    /// groups, so shares can add up to more than 100%.
    pub multi_valued: bool,
    /// Events matching the filters; the base for every percentage.
    pub total_events: usize,
    pub groups: Vec<GroupCount>,
    /// Groups left out because of `top`, and the events in them.
    pub other_groups: usize,
    pub other_events: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct GroupCount {
    /// Absent for events with no value: unclassified, no tags, or no date.
    pub value: Option<String>,
    pub events: usize,
    /// Percent of `total_events` (for a subgroup: of its parent group), one decimal.
    pub pct: f64,
    /// Breakdown of this group by `then_by`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by: Vec<GroupCount>,
    /// Subgroups left out because of `top`.
    #[serde(skip_serializing_if = "is_zero")]
    pub more: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Report text for quoting: the licensee's own words (OCR, so expect some garbling).
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct EventText {
    pub accession_number: String,
    pub ler_number: Option<String>,
    pub docket: Option<String>,
    pub reporting_unit: Option<String>,
    pub title: Option<String>,
    pub event_date: Option<String>,
    pub pdf_url: Option<String>,
    pub sections: Vec<TextSection>,
    /// Requested sections the parser didn't find in this report.
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct TextSection {
    /// abstract, condition_prior, description, cause, safety, corrective, previous, narrative.
    pub name: String,
    pub text: String,
    /// Full length of the section in characters, before any truncation.
    pub chars: usize,
    pub truncated: bool,
}

/// How current the database is, loaded once at startup and shown in the server instructions so
/// the model can say "as of" and knows what "latest" can mean.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DataInfo {
    pub events: i64,
    pub classified: i64,
    /// Newest report date among events (dates after today, from bad scans, are ignored).
    pub newest_report: Option<String>,
    /// Newest date a document was added to ADAMS.
    pub newest_added: Option<String>,
    pub built_at: Option<String>,
    pub parser_version: Option<String>,
    pub coverage: Coverage,
}

impl fmt::Display for DataInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Data as of: newest report dated {}, ADAMS documents added through {}. \
             {} events, {} classified. Database built {}",
            or_unknown(&self.newest_report),
            or_unknown(&self.newest_added),
            self.events,
            self.classified,
            or_unknown(&self.built_at),
        )?;
        if let Some(v) = &self.parser_version {
            write!(f, " (parser v{v})")?;
        }
        write!(
            f,
            ". LERs are due 60 days after discovery, so the most recent \
             couple of months are incomplete; say so when asked about \"latest\" or recent counts."
        )?;
        if !self.coverage.by_year.is_empty() {
            write!(f, "\n\n{}", self.coverage)?;
        }
        Ok(())
    }
}

/// Events a year needs to count as well covered. Full years since 1999 have 120-620.
pub const WELL_COVERED: i64 = 100;

/// Why the sparse years are sparse. Update this if the ADAMS legacy library is pulled.
const SPARSE_REASON: &str = "only the main ADAMS library was pulled, and most older LERs are \
                             in its legacy library";

/// Events per event year (falling back to report year), to tell the model which years the
/// database covers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Coverage {
    /// (year, events), ascending; years with no events may be absent.
    pub by_year: Vec<(i32, i64)>,
    /// The year still being reported (the newest report's year), never counted as sparse.
    pub current_year: Option<i32>,
}

impl Coverage {
    fn count(&self, year: i32) -> i64 {
        self.by_year
            .iter()
            .find(|(y, _)| *y == year)
            .map_or(0, |(_, n)| *n)
    }

    /// Runs of consecutive years, well covered or not, from the first year with data to the
    /// last full year: (first, last, well covered, events).
    fn spans(&self) -> Vec<(i32, i32, bool, i64)> {
        let (Some(first), Some(last)) = (self.by_year.first(), self.by_year.last()) else {
            return vec![];
        };
        let last = match self.current_year {
            Some(c) if c <= last.0 => c - 1,
            _ => last.0,
        };
        let mut spans: Vec<(i32, i32, bool, i64)> = vec![];
        for year in first.0..=last {
            let n = self.count(year);
            let good = n >= WELL_COVERED;
            match spans.last_mut() {
                Some(s) if s.2 == good => {
                    s.1 = year;
                    s.3 += n;
                }
                _ => spans.push((year, year, good, n)),
            }
        }
        spans
    }
}

fn year_span(first: i32, last: i32) -> String {
    if first == last {
        first.to_string()
    } else {
        format!("{first}-{last}")
    }
}

impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spans = self.spans();
        let list = |good: bool| -> Vec<String> {
            spans
                .iter()
                .filter(|s| s.2 == good)
                .map(|s| year_span(s.0, s.1))
                .collect()
        };
        let (covered, sparse) = (list(true), list(false));
        write!(f, "Coverage by event year: ")?;
        if covered.is_empty() {
            write!(f, "no year has {WELL_COVERED}+ reports")?;
        } else {
            write!(
                f,
                "{} well covered ({WELL_COVERED}+ reports a year)",
                covered.join(", ")
            )?;
        }
        if let Some(c) = self.current_year {
            write!(f, "; {c} still being reported ({} so far)", self.count(c))?;
        }
        write!(f, ".")?;
        if !sparse.is_empty() {
            let n: i64 = spans.iter().filter(|s| !s.2).map(|s| s.3).sum();
            write!(
                f,
                " Sparse: {} ({n} reports in all), because {SPARSE_REASON}. An event from a \
                 sparse year that isn't found is most likely missing from this database, not \
                 nonexistent: say so, and point to the NRC (ADAMS or nrc.gov) for it.",
                sparse.join(", ")
            )?;
        }
        Ok(())
    }
}

fn or_unknown<T: AsRef<str>>(v: &Option<T>) -> &str {
    v.as_ref().map_or("unknown", AsRef::as_ref)
}

/// "a, b, c".
fn join<T: AsRef<str>>(values: &[T]) -> String {
    values
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join(", ")
}

/// "05000327/2014-002-00", or whichever part is known.
fn ler_label(docket: &Option<String>, ler_number: &Option<String>) -> String {
    match (docket, ler_number) {
        (Some(d), Some(n)) => format!("{d}/{n}"),
        (None, Some(n)) => n.clone(),
        (_, None) => "(no LER number)".to_string(),
    }
}

impl fmt::Display for EventReport {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(
            f,
            "LER {} ({})",
            ler_label(&self.docket, &self.ler_number),
            self.accession_number
        )?;
        writeln!(f, "{}", or_unknown(&self.title))?;
        writeln!(
            f,
            "Site: {}, {} ({})",
            or_unknown(&self.site),
            or_unknown(&self.reporting_unit),
            or_unknown(&self.reactor_type)
        )?;
        writeln!(
            f,
            "Event date: {}, report date: {}",
            or_unknown(&self.event_date),
            or_unknown(&self.report_date)
        )?;
        if let Some(url) = &self.pdf_url {
            writeln!(f, "PDF: {url}")?;
        }
        if let Some(p) = &self.precursor {
            writeln!(f, "{p}")?;
        }
        match &self.classification {
            Some(c) => write!(f, "\n{c}")?,
            None => writeln!(f, "\nNot classified.")?,
        }
        if !self.notes.is_empty() {
            writeln!(f, "\nNotes:")?;
            for n in &self.notes {
                writeln!(f, "- {n}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for Classification {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "Summary: {}", or_unknown(&self.summary))?;
        writeln!(
            f,
            "Cause: {}; human error: {}; error type: {}",
            or_unknown(&self.primary_cause),
            or_unknown(&self.human_error),
            or_unknown(&self.error_type)
        )?;
        if !self.who.is_empty() {
            writeln!(f, "  Who: {}", join(&self.who))?;
        }
        if !self.operator_fundamentals.is_empty() {
            writeln!(
                f,
                "  Operator fundamentals: {}",
                join(&self.operator_fundamentals)
            )?;
        }
        write!(f, "System: {}", or_unknown(&self.system_name))?;
        if let Some(code) = &self.eiis_code {
            write!(f, " [EIIS {code}]")?;
        }
        writeln!(
            f,
            "; component: {}; failure mode: {}; mechanism: {}",
            or_unknown(&self.component),
            or_unknown(&self.failure_mode),
            or_unknown(&self.failure_mechanism)
        )?;
        writeln!(f, "{}", self.barriers)?;
        write!(
            f,
            "Radiological release: {}",
            or_unknown(&self.radiological_release)
        )?;
        match &self.release_pathway {
            Some(p) => writeln!(f, " ({p})")?,
            None => writeln!(f)?,
        }
        if !self.non_radiological_release.is_empty() {
            write!(
                f,
                "Non-radiological release: {}",
                join(&self.non_radiological_release)
            )?;
            match &self.non_radiological_description {
                Some(d) => writeln!(f, " ({d})")?,
                None => writeln!(f)?,
            }
        }
        writeln!(f, "Trip risk: {}", or_unknown(&self.trip_risk))?;
        if !self.generation.is_empty() {
            writeln!(f, "Generation:")?;
            for u in &self.generation {
                writeln!(f, "  {u}")?;
            }
        }
        writeln!(
            f,
            "Classified by {}, confidence {}",
            self.source,
            or_unknown(&self.confidence)
        )
    }
}

impl fmt::Display for Barriers {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Barriers: fuel cladding {}, RCS pressure boundary {}, containment {}",
            or_unknown(&self.fuel_cladding),
            or_unknown(&self.rcs_pressure_boundary),
            or_unknown(&self.containment)
        )
    }
}

impl fmt::Display for UnitImpact {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.unit_name)?;
        if self.event_occurred_here {
            write!(f, " [event unit]")?;
        }
        write!(f, ": {}", or_unknown(&self.impact))?;
        match (self.power_before_pct, self.power_after_pct) {
            (Some(b), Some(a)) => write!(f, ", {b}% -> {a}%")?,
            (Some(b), None) => write!(f, ", from {b}%")?,
            (None, Some(a)) => write!(f, ", to {a}%")?,
            (None, None) => {}
        }
        if let Some(h) = self.duration_hours {
            write!(f, ", {h} h")?;
        }
        Ok(())
    }
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} ({}) {}: {}",
            ler_label(&self.docket, &self.ler_number),
            self.accession_number,
            or_unknown(&self.reporting_unit),
            or_unknown(&self.title)
        )
    }
}

impl fmt::Display for Candidates {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(
            f,
            "{} LERs match. Call again with one of these accession or LER numbers:",
            self.total
        )?;
        for c in &self.shown {
            writeln!(f, "- {c}")?;
        }
        if self.total > self.shown.len() {
            writeln!(
                f,
                "... and {} more. Add the docket to narrow it down, e.g. \"327-2014-002\" for Sequoyah Unit 1.",
                self.total - self.shown.len()
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for EventSummary {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let date = match (&self.event_date, &self.report_date) {
            (Some(d), _) => d.clone(),
            (None, Some(r)) => format!("reported {r}"),
            (None, None) => "date unknown".into(),
        };
        writeln!(
            f,
            "{date}  {} ({}) {}",
            ler_label(&self.docket, &self.ler_number),
            self.accession_number,
            self.reporting_unit
                .as_deref()
                .or(self.site.as_deref())
                .unwrap_or("unknown plant")
        )?;
        writeln!(f, "  {}", or_unknown(&self.title))?;
        if self.system_name.is_some() || self.eiis_code.is_some() {
            write!(f, "  System: {}", or_unknown(&self.system_name))?;
            if let Some(code) = &self.eiis_code {
                write!(f, " [EIIS {code}]")?;
            }
            writeln!(
                f,
                "; cause: {}; failure mode: {}",
                or_unknown(&self.primary_cause),
                or_unknown(&self.failure_mode)
            )?;
        }
        if matches!(
            self.human_error,
            Some(HumanError::Yes | HumanError::Contributing)
        ) {
            write!(f, "  Human error: {}", or_unknown(&self.human_error))?;
            if !self.who.is_empty() {
                write!(f, "; who: {}", join(&self.who))?;
            }
            if !self.operator_fundamentals.is_empty() {
                write!(
                    f,
                    "; operator fundamentals: {}",
                    join(&self.operator_fundamentals)
                )?;
            }
            writeln!(f)?;
        }
        if let Some(n) = self.severity.filter(|n| *n > 0) {
            write!(f, "  Severity score {n}")?;
            if !self.barriers.is_empty() {
                write!(f, "; {}", self.barriers.join(", "))?;
            }
            if !self.safety_functions_lost.is_empty() {
                write!(
                    f,
                    "; safety functions lost: {}",
                    join(&self.safety_functions_lost)
                )?;
            }
            if !self.reported_under.is_empty() {
                write!(f, "; reported as: {}", join(&self.reported_under))?;
            }
            writeln!(f)?;
        }
        if self.radiological_release.is_some() || !self.non_radiological_release.is_empty() {
            write!(f, "  Environmental release:")?;
            if let Some(r) = &self.radiological_release {
                write!(f, " radiological {r}")?;
            }
            if !self.non_radiological_release.is_empty() {
                if self.radiological_release.is_some() {
                    write!(f, ";")?;
                }
                write!(f, " {}", join(&self.non_radiological_release))?;
            }
            writeln!(f)?;
        }
        if let Some(p) = &self.precursor {
            writeln!(f, "  {p}")?;
        }
        if let Some(s) = &self.summary {
            writeln!(f, "  {s}")?;
        }
        Ok(())
    }
}

/// NRC precursors, riskiest first, whether or not their LER is in this database.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct PrecursorList {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Precursors matching the filters.
    pub total: usize,
    pub results: Vec<PrecursorEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct PrecursorEntry {
    /// "05000259/1975-006": docket and LER number the NRC lists.
    pub ler: String,
    /// Unit name, e.g. "Browns Ferry Unit 1".
    pub plant: Option<String>,
    pub event_date: Option<String>,
    pub precursor: Precursor,
    /// The LER in this database, if it is here (for get_event / get_event_text).
    pub accession_number: Option<String>,
    pub title: Option<String>,
}

impl fmt::Display for PrecursorList {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for n in &self.notes {
            writeln!(f, "{n}")?;
        }
        writeln!(
            f,
            "{} NRC precursors match; showing {}, riskiest first.\n",
            self.total,
            self.results.len()
        )?;
        for e in &self.results {
            writeln!(
                f,
                "{}  {} {}",
                e.event_date.as_deref().unwrap_or("date unknown"),
                e.ler,
                e.plant.as_deref().unwrap_or("unknown plant")
            )?;
            writeln!(f, "  {}", e.precursor)?;
            match (&e.accession_number, &e.title) {
                (Some(a), t) => writeln!(f, "  LER {a}: {}", or_unknown(t))?,
                (None, _) => writeln!(
                    f,
                    "  LER not in this database (pre-1999 reports are in the ADAMS legacy library)"
                )?,
            }
        }
        Ok(())
    }
}

/// "6e-3" style.
fn sci(v: f64) -> String {
    format!("{v:.0e}")
}

impl fmt::Display for Precursor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "NRC precursor analysis (ASP): ")?;
        match (self.ccdp, &self.finding) {
            (Some(v), _) => {
                write!(
                    f,
                    "CCDP/delta-CDP {} (about 1 in {:.0} chance of core damage)",
                    sci(v),
                    1.0 / v
                )?;
                if self.significant {
                    write!(f, ", a significant precursor (1e-3 or more)")?;
                }
            }
            (None, Some(c)) => write!(f, "{c} inspection finding (no number published)")?,
            (None, None) => write!(f, "precursor, value not parsed")?,
        }
        write!(f, "; listed in {} NRC document(s)", self.sources)?;
        if !self.other_estimates.is_empty() {
            let other: Vec<String> = self.other_estimates.iter().map(|v| sci(*v)).collect();
            write!(f, "; earlier estimates {}", other.join(", "))?;
        }
        Ok(())
    }
}

impl fmt::Display for SearchResults {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for n in &self.notes {
            writeln!(f, "{n}")?;
        }
        if self.results.is_empty() {
            return writeln!(f, "No events match.");
        }
        writeln!(
            f,
            "{} events match; showing {}. Use get_event with an accession number for details.",
            self.total,
            self.results.len()
        )?;
        for r in &self.results {
            write!(f, "\n{r}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Breakdown {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for n in &self.notes {
            writeln!(f, "{n}")?;
        }
        write!(
            f,
            "{} events matched, by {}",
            self.total_events, self.group_by
        )?;
        match &self.then_by {
            Some(t) => writeln!(f, ", then {t} (as % of each group):")?,
            None => writeln!(f, ":")?,
        }
        let width = self
            .groups
            .iter()
            .map(|g| g.value.as_deref().map_or(6, str::len))
            .max()
            .unwrap_or(0);
        for g in &self.groups {
            writeln!(
                f,
                "  {:<width$}  {:>6}  {:>5.1}%",
                g.value.as_deref().unwrap_or("(none)"),
                g.events,
                g.pct
            )?;
            if !g.by.is_empty() {
                let parts: Vec<String> =
                    g.by.iter()
                        .map(|c| {
                            format!(
                                "{} {} ({:.1}%)",
                                c.value.as_deref().unwrap_or("(none)"),
                                c.events,
                                c.pct
                            )
                        })
                        .collect();
                write!(f, "      {}", parts.join(", "))?;
                if g.more > 0 {
                    write!(f, ", ... {} more", g.more)?;
                }
                writeln!(f)?;
            }
        }
        if self.other_groups > 0 {
            writeln!(
                f,
                "  ... {} more groups with {} events",
                self.other_groups, self.other_events
            )?;
        }
        if self.multi_valued {
            writeln!(
                f,
                "An event can be in several groups, so shares can add up to more than 100%. \
                 (none) = nothing recorded for that field (for who: no human error)."
            )?;
        } else {
            writeln!(f, "(none) = no value: not classified, or no date.")?;
        }
        Ok(())
    }
}

impl fmt::Display for EventText {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(
            f,
            "LER {} ({}) {}, event date {}",
            ler_label(&self.docket, &self.ler_number),
            self.accession_number,
            or_unknown(&self.reporting_unit),
            or_unknown(&self.event_date)
        )?;
        writeln!(f, "{}", or_unknown(&self.title))?;
        if let Some(url) = &self.pdf_url {
            writeln!(f, "PDF: {url}")?;
        }
        writeln!(
            f,
            "Text is OCR of the licensee's report; check the PDF before quoting exact figures."
        )?;
        for s in &self.sections {
            writeln!(f, "\n## {}", s.name)?;
            writeln!(f, "{}", s.text)?;
            if s.truncated {
                writeln!(
                    f,
                    "[truncated: {} of {} characters; ask for this section alone or raise max_chars]",
                    s.text.chars().count(),
                    s.chars
                )?;
            }
        }
        if self.sections.is_empty() {
            writeln!(f, "\nNo text was parsed for this report; use the PDF.")?;
        }
        if !self.missing.is_empty() {
            writeln!(f, "\nNot found in this report: {}", self.missing.join(", "))?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn precursor_display() {
        let p = Precursor {
            ccdp: Some(6e-3),
            finding: None,
            significant: true,
            sources: 12,
            other_estimates: vec![2e-2],
        };
        assert_eq!(
            p.to_string(),
            "NRC precursor analysis (ASP): CCDP/delta-CDP 6e-3 (about 1 in 167 chance of core damage), \
             a significant precursor (1e-3 or more); listed in 12 NRC document(s); earlier estimates 2e-2"
        );
        let w = Precursor {
            ccdp: None,
            finding: Some("White".into()),
            significant: false,
            sources: 1,
            other_estimates: vec![],
        };
        assert_eq!(
            w.to_string(),
            "NRC precursor analysis (ASP): White inspection finding (no number published); listed in 1 NRC document(s)"
        );
    }

    #[test]
    fn data_info_display() {
        let d = DataInfo {
            events: 9463,
            classified: 9205,
            newest_report: s("2026-11-02"),
            newest_added: s("2026-11-10 14:35"),
            built_at: s("2026-11-15T14:35:09"),
            parser_version: s("5"),
            coverage: Coverage::default(),
        };
        let t = d.to_string();
        assert!(t.starts_with("Data as of: newest report dated 2026-11-02, ADAMS documents added through 2026-11-10 14:35."));
        assert!(t.contains(
            "9463 events, 9205 classified. Database built 2026-11-15T14:35:09 (parser v5)."
        ));
        let unknown = DataInfo::default().to_string();
        assert!(unknown.contains("newest report dated unknown"));
        assert!(!unknown.contains("parser v"));
    }

    #[test]
    fn coverage_spans_and_text() {
        let c = Coverage {
            by_year: vec![
                (1976, 340),
                (1977, 433),
                (1980, 46),
                (1999, 270),
                (2000, 618),
                (2001, 40),
            ],
            current_year: Some(2001),
        };
        assert_eq!(
            c.spans(),
            [
                (1976, 1977, true, 773),
                (1978, 1998, false, 46),
                (1999, 2000, true, 888)
            ]
        );
        let t = c.to_string();
        assert!(
            t.starts_with(
                "Coverage by event year: 1976-1977, 1999-2000 well covered (100+ reports a year); \
             2001 still being reported (40 so far). Sparse: 1978-1998 (46 reports in all), because"
            ),
            "{t}"
        );
        let full = Coverage {
            by_year: vec![(2020, 136), (2021, 140)],
            current_year: None,
        };
        assert_eq!(
            full.to_string(),
            "Coverage by event year: 2020-2021 well covered (100+ reports a year)."
        );
        assert!(Coverage::default().spans().is_empty());
    }

    pub(crate) fn sample_report() -> EventReport {
        EventReport {
            accession_number: "ML14219A568".into(),
            ler_number: s("2014-002-00"),
            docket: s("05000327"),
            title: s("Containment penetrations open during fuel movement"),
            site: s("Sequoyah"),
            reporting_unit: s("Sequoyah Unit 1"),
            reactor_type: Some(ReactorType::Pwr),
            event_date: s("2014-06-06"),
            report_date: s("2014-08-05"),
            pdf_url: None,
            classification: Some(Classification {
                summary: s("Penetrations opened without required controls."),
                primary_cause: Some(Cause::Procedure),
                human_error: Some(HumanError::Contributing),
                error_type: s("procedure_inadequate"),
                who: vec![Who::Maintenance],
                operator_fundamentals: vec![],
                system_name: s("containment isolation"),
                eiis_code: s("NH"),
                component: None,
                failure_mode: s("no_equipment_failure"),
                failure_mechanism: None,
                barriers: Barriers {
                    fuel_cladding: Some(BarrierState::None),
                    rcs_pressure_boundary: Some(BarrierState::None),
                    containment: Some(BarrierState::Degraded),
                },
                radiological_release: Some(RadiologicalRelease::None),
                release_pathway: None,
                non_radiological_release: vec![],
                non_radiological_description: None,
                trip_risk: s("none"),
                generation: vec![
                    UnitImpact {
                        unit_name: "Sequoyah Unit 2".into(),
                        docket: s("05000328"),
                        event_occurred_here: true,
                        impact: s("already_shutdown"),
                        power_before_pct: Some(0),
                        power_after_pct: Some(0),
                        duration_hours: None,
                    },
                    UnitImpact {
                        unit_name: "Sequoyah Unit 1".into(),
                        docket: s("05000327"),
                        event_occurred_here: false,
                        impact: s("none"),
                        power_before_pct: None,
                        power_after_pct: None,
                        duration_hours: None,
                    },
                ],
                confidence: s("high"),
                source: "p4 / claude-haiku-5-5".into(),
            }),
            precursor: None,
            notes: vec![],
        }
    }

    #[test]
    fn report_display() {
        let text = sample_report().to_string();
        let expected = "\
LER 05000327/2014-002-00 (ML14219A568)
Containment penetrations open during fuel movement
Site: Sequoyah, Sequoyah Unit 1 (PWR)
Event date: 2014-06-06, report date: 2014-08-05

Summary: Penetrations opened without required controls.
Cause: procedure; human error: contributing; error type: procedure_inadequate
  Who: maintenance
System: containment isolation [EIIS NH]; component: unknown; failure mode: no_equipment_failure; mechanism: unknown
Barriers: fuel cladding none, RCS pressure boundary none, containment degraded
Radiological release: none
Trip risk: none
Generation:
  Sequoyah Unit 2 [event unit]: already_shutdown, 0% -> 0%
  Sequoyah Unit 1: none
Classified by p4 / claude-haiku-5-5, confidence high
";
        assert_eq!(text, expected);
    }

    #[test]
    fn report_display_releases() {
        let mut r = sample_report();
        let c = r.classification.as_mut().unwrap();
        c.radiological_release = Some(RadiologicalRelease::UnmonitoredPath);
        c.release_pathway = s("condensate spill to yard and storm drain");
        c.non_radiological_release = vec![NonRadiologicalRelease::OilChemicalSpill];
        c.non_radiological_description = s("lube oil to storm drain");
        let text = r.to_string();
        assert!(
            text.contains(
                "Radiological release: unmonitored_path (condensate spill to yard and storm drain)\n\
                 Non-radiological release: oil_chemical_spill (lube oil to storm drain)\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn search_hit_shows_releases() {
        let mut hit = sample_hit();
        hit.radiological_release = Some(RadiologicalRelease::WithinLimits);
        hit.non_radiological_release = vec![NonRadiologicalRelease::ThermalDischarge];
        let text = SearchResults { notes: vec![], total: 1, results: vec![hit] }.to_string();
        assert!(
            text.contains("  Environmental release: radiological within_limits; thermal_discharge\n"),
            "{text}"
        );
        assert!(!text.contains("release: within_limits;"), "release not repeated in severity line: {text}");
    }

    #[test]
    fn report_unclassified_with_notes() {
        let mut r = sample_report();
        r.classification = None;
        r.ler_number = None;
        r.title = None;
        r.pdf_url = s("https://example.test/a.pdf");
        r.notes = vec!["superseded by revision 01 (ML00000000001)".into()];
        let text = r.to_string();
        assert!(
            text.starts_with("LER (no LER number) (ML14219A568)\nunknown\n"),
            "{text}"
        );
        assert!(text.contains("PDF: https://example.test/a.pdf\n"));
        assert!(text.contains("\nNot classified.\n"));
        assert!(
            text.ends_with("\nNotes:\n- superseded by revision 01 (ML00000000001)\n"),
            "{text}"
        );
    }

    #[test]
    fn unit_impact_power_and_duration() {
        let mut u = UnitImpact {
            unit_name: "Unit 1".into(),
            docket: None,
            event_occurred_here: false,
            impact: s("trip"),
            power_before_pct: Some(100),
            power_after_pct: None,
            duration_hours: Some(36.5),
        };
        assert_eq!(u.to_string(), "Unit 1: trip, from 100%, 36.5 h");
        u.power_before_pct = None;
        u.power_after_pct = Some(0);
        u.impact = None;
        assert_eq!(u.to_string(), "Unit 1: unknown, to 0%, 36.5 h");
    }

    #[test]
    fn candidates_display() {
        let c = |acc: &str, d: &str, unit: &str| Candidate {
            accession_number: acc.into(),
            ler_number: s("2014-002-00"),
            docket: s(d),
            reporting_unit: s(unit),
            title: s("Some title"),
        };
        let both = vec![
            c("ML1", "05000327", "Sequoyah Unit 1"),
            c("ML2", "05000328", "Sequoyah Unit 2"),
        ];
        let text = Candidates {
            shown: both.clone(),
            total: 2,
        }
        .to_string();
        assert_eq!(
            text,
            "\
2 LERs match. Call again with one of these accession or LER numbers:
- 05000327/2014-002-00 (ML1) Sequoyah Unit 1: Some title
- 05000328/2014-002-00 (ML2) Sequoyah Unit 2: Some title
"
        );
        let capped = Candidates {
            shown: both,
            total: 72,
        }
        .to_string();
        assert!(capped.starts_with("72 LERs match."), "{capped}");
        assert!(capped.ends_with("... and 70 more. Add the docket to narrow it down, e.g. \"327-2014-002\" for Sequoyah Unit 1.\n"), "{capped}");
    }

    fn sample_hit() -> EventSummary {
        EventSummary {
            accession_number: "ML26181A193".into(),
            ler_number: s("2026-001-00"),
            docket: s("05000327"),
            site: s("Sequoyah"),
            reporting_unit: s("Sequoyah Unit 1"),
            event_date: None,
            report_date: s("2026-03-20"),
            title: s("AFW actuation"),
            system_name: s("auxiliary feedwater"),
            eiis_code: s("BA"),
            primary_cause: Some(Cause::Equipment),
            failure_mode: s("spurious_actuation"),
            human_error: Some(HumanError::Contributing),
            who: vec![Who::LicensedOperator],
            operator_fundamentals: vec![
                OperatorFundamental::Knowledge,
                OperatorFundamental::Monitoring,
            ],
            summary: s("Pumps started."),
            barriers: vec!["containment: potential".into()],
            radiological_release: None,
            non_radiological_release: vec![],
            safety_functions_lost: vec![SafetyFunction::HeatRemoval],
            reported_under: vec![Criterion::SafetyFunctionFailure],
            precursor: None,
            severity: Some(6),
        }
    }

    #[test]
    fn search_results_display() {
        let hit = sample_hit();
        let text = SearchResults {
            notes: vec![],
            total: 3,
            results: vec![hit],
        }
        .to_string();
        assert_eq!(
            text,
            "\
3 events match; showing 1. Use get_event with an accession number for details.

reported 2026-03-20  05000327/2026-001-00 (ML26181A193) Sequoyah Unit 1
  AFW actuation
  System: auxiliary feedwater [EIIS BA]; cause: equipment; failure mode: spurious_actuation
  Human error: contributing; who: licensed_operator; operator fundamentals: knowledge, monitoring
  Severity score 6; containment: potential; safety functions lost: heat_removal; reported as: safety_function_failure
  Pumps started.
"
        );
        let none = SearchResults {
            notes: vec![],
            total: 0,
            results: vec![],
        };
        assert_eq!(none.to_string(), "No events match.\n");
    }

    #[test]
    fn breakdown_display() {
        let g = |v: Option<&str>, events, pct| GroupCount {
            value: v.map(Into::into),
            events,
            pct,
            by: vec![],
            more: 0,
        };
        let mut b = Breakdown {
            notes: vec![],
            group_by: "cause".into(),
            then_by: None,
            multi_valued: false,
            total_events: 10,
            groups: vec![g(Some("equipment"), 6, 60.0), g(None, 3, 30.0)],
            other_groups: 1,
            other_events: 1,
        };
        assert_eq!(
            b.to_string(),
            "\
10 events matched, by cause:
  equipment       6   60.0%
  (none)          3   30.0%
  ... 1 more groups with 1 events
(none) = no value: not classified, or no date.
"
        );
        b.then_by = Some("decade".into());
        b.groups[0].by = vec![g(Some("1970s"), 4, 66.7), g(Some("2020s"), 2, 33.3)];
        b.groups[0].more = 2;
        let text = b.to_string();
        assert!(
            text.starts_with("10 events matched, by cause, then decade (as % of each group):\n"),
            "{text}"
        );
        assert!(
            text.contains("\n      1970s 4 (66.7%), 2020s 2 (33.3%), ... 2 more\n"),
            "{text}"
        );
    }

    #[test]
    fn report_serializes_to_schema_shape() {
        let v = serde_json::to_value(sample_report()).unwrap();
        assert_eq!(v["classification"]["barriers"]["containment"], "degraded");
        assert_eq!(
            v["classification"]["generation"][0]["event_occurred_here"],
            true
        );
        assert!(v["pdf_url"].is_null());
    }
}
