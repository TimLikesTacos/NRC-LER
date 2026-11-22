//! The classifier's vocabularies (prompt p4) and the reporting criteria, as enums. Tool arguments
//! deserialize straight into these and tool output serializes from them, so the JSON Schemas list
//! the allowed values. `as_str` is the value as stored in the database; the classifier's
//! structured output (classify_lers.py) allows only these, so the two must be kept in step.

use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A stored value that isn't in the vocabulary: vocab.rs is behind the classifier prompt.
#[derive(Debug)]
pub struct UnknownValue {
    kind: &'static str,
    value: String,
}

impl fmt::Display for UnknownValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?} is not a known {} value", self.value, self.kind)
    }
}

impl std::error::Error for UnknownValue {}

/// An enum whose variants are the given strings, with `as_str`, `ALL` (in declaration order),
/// `AsRef<str>`, `Display` and `FromSql`. Attributes on a variant (doc comments,
/// `#[serde(alias = ...)]`) are kept.
macro_rules! vocab {
    (
        $(#[$meta:meta])*
        $name:ident { $( $(#[$vmeta:meta])* $variant:ident = $s:literal, )+ }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
        pub enum $name {
            $( $(#[$vmeta])* #[serde(rename = $s)] $variant, )+
        }

        impl $name {
            pub const ALL: &[Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $s,)+
                }
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let s = value.as_str()?;
                Self::ALL.iter().copied().find(|v| v.as_str() == s).ok_or_else(|| {
                    FromSqlError::Other(Box::new(UnknownValue {
                        kind: stringify!($name),
                        value: s.to_string(),
                    }))
                })
            }
        }
    };
}

vocab! {
    ReactorType {
        #[serde(alias = "pwr")]
        Pwr = "PWR",
        #[serde(alias = "bwr")]
        Bwr = "BWR",
        #[serde(alias = "fbr")]
        Fbr = "FBR",
        Research = "research",
        /// Spent fuel storage at a reactor site, for reports that name no reactor unit.
        #[serde(alias = "isfsi")]
        Isfsi = "ISFSI",
    }
}

vocab! {
    Cause {
        Equipment = "equipment",
        HumanPerformance = "human_performance",
        Design = "design",
        Procedure = "procedure",
        External = "external",
        Unknown = "unknown",
    }
}

vocab! {
    HumanError {
        Yes = "yes",
        Contributing = "contributing",
        No = "no",
        Unknown = "unknown",
    }
}

vocab! {
    Who {
        LicensedOperator = "licensed_operator",
        NonLicensedOperator = "non_licensed_operator",
        Maintenance = "maintenance",
        IAndC = "i_and_c",
        Engineering = "engineering",
        Management = "management",
        Contractor = "contractor",
        Chemistry = "chemistry",
        RadiationProtection = "radiation_protection",
        Security = "security",
        Other = "other",
    }
}

vocab! {
    OperatorFundamental {
        Monitoring = "monitoring",
        Control = "control",
        Conservatism = "conservatism",
        Teamwork = "teamwork",
        Knowledge = "knowledge",
    }
}

vocab! {
    /// Fission product barrier condition, least to most severe.
    BarrierState {
        None = "none",
        Potential = "potential",
        Challenged = "challenged",
        Degraded = "degraded",
        Breached = "breached",
    }
}

vocab! {
    /// Radioactive material the event put into the environment (second-pass release check,
    /// classify_release.py). Barrier problems and leaks that stayed inside the plant, routine
    /// effluent, and lost sources are "none".
    RadiologicalRelease {
        None = "none",
        /// Unplanned release through a monitored effluent path (plant vent, stack, air
        /// ejector, monitored discharge), within limits.
        WithinLimits = "within_limits",
        /// A release the LER says exceeded a regulatory or Technical Specification limit.
        AboveLimits = "above_limits",
        /// Reached the environment by a path not monitored for it: spills to the yard or
        /// storm drains, tritium in groundwater, leaks into unmonitored discharges.
        UnmonitoredPath = "unmonitored_path",
        Unknown = "unknown",
    }
}

vocab! {
    /// Non-radioactive material or heat the event put into the environment (second-pass
    /// release check, classify_release.py).
    NonRadiologicalRelease {
        /// Oil, fuel, chemicals or sewage that reached soil, groundwater, storm drains or
        /// surface water, or a toxic gas or refrigerant release to the air. Leaks contained
        /// inside a building or a dike are not counted.
        OilChemicalSpill = "oil_chemical_spill",
        /// Cooling water discharged above a thermal permit or Technical Specification limit.
        ThermalDischarge = "thermal_discharge",
        /// A discharge over an NPDES or other environmental permit limit, other than thermal.
        PermitExceedance = "permit_exceedance",
        Other = "other",
    }
}

vocab! {
    SafetyFunction {
        Shutdown = "shutdown",
        HeatRemoval = "heat_removal",
        ReleaseControl = "release_control",
        AccidentMitigation = "accident_mitigation",
    }
}

// Doc comments on these enums and their variants end up in the tools' JSON Schema, so they are
// written for the model. Criteria are matched against the sections parsed into `ler_cfr`.
vocab! {
    /// Reporting criteria the licensee cites (10 CFR 50.72 / 50.73), by a plain name. The
    /// licensee's own call, so a steadier severity signal than the AI barrier ratings.
    Criterion {
        /// An emergency class was declared.
        EmergencyDeclared = "emergency_declared",
        /// Plant or a principal safety barrier seriously degraded.
        SeriouslyDegraded = "seriously_degraded",
        /// Unanalyzed condition that significantly degraded safety.
        UnanalyzedCondition = "unanalyzed_condition",
        /// A safety function could have been prevented.
        SafetyFunctionFailure = "safety_function_failure",
        /// An actual threat to safety or site personnel (fire, toxic gas, ...).
        ActualThreat = "actual_threat",
        /// A natural phenomenon or other external condition threatened safety.
        ExternalThreat = "external_threat",
        /// An airborne or liquid radioactive release.
        RadioactiveRelease = "radioactive_release",
        /// A shutdown required by Technical Specifications.
        TsShutdown = "ts_shutdown",
        /// Operation or a condition prohibited by Technical Specifications.
        TsProhibited = "ts_prohibited",
        /// A reactor trip or safety system actuation.
        SafetySystemActuation = "safety_system_actuation",
        /// One cause made independent trains inoperable.
        CommonCause = "common_cause",
    }
}

impl Criterion {
    /// The CFR sections that cite this criterion, lowercase as matched against `ler_cfr`; each
    /// also matches its subsections (see `matches_section`).
    pub fn cfrs(self) -> &'static [&'static str] {
        match self {
            // 50.72(a)(1)(i) is the declaration itself and (a)(3) the notification that follows
            // one. Licensees also cite it as plain (a)(1), and scans misread it as (a)(i)(i) or
            // (1)(i). Not (a)(2): that is a miscited 50.73 or 50.72(b) item, almost never an
            // emergency.
            Self::EmergencyDeclared => {
                &["50.72(a)(1)", "50.72(a)(3)", "50.72(a)(i)", "50.72(1)(i)"]
            }
            Self::SeriouslyDegraded => &["50.73(a)(2)(ii)(a)"],
            Self::UnanalyzedCondition => &["50.73(a)(2)(ii)(b)"],
            Self::SafetyFunctionFailure => &["50.73(a)(2)(v)"],
            Self::ActualThreat => &["50.73(a)(2)(x)"],
            Self::ExternalThreat => &["50.73(a)(2)(iii)"],
            Self::RadioactiveRelease => &["50.73(a)(2)(viii)"],
            Self::TsShutdown => &["50.73(a)(2)(i)(a)"],
            Self::TsProhibited => &["50.73(a)(2)(i)(b)"],
            Self::SafetySystemActuation => &["50.73(a)(2)(iv)"],
            Self::CommonCause => &["50.73(a)(2)(vii)"],
        }
    }

    /// Whether a cited section (lowercase) is one of `cfrs` or a subsection of one, so
    /// "50.73(a)(2)(v)" matches "(v)(d)" but not "(vii)".
    pub fn matches_section(self, section: &str) -> bool {
        self.cfrs().iter().any(|cfr| {
            section
                .strip_prefix(cfr)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('('))
        })
    }

    /// Points toward the severity score.
    pub fn points(self) -> i64 {
        match self {
            Self::EmergencyDeclared | Self::SeriouslyDegraded => 6,
            Self::SafetyFunctionFailure | Self::ActualThreat => 3,
            Self::UnanalyzedCondition | Self::ExternalThreat => 2,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_stored_names_and_reactor_aliases() {
        let c: Vec<Criterion> = serde_json::from_str(r#"["common_cause", "ts_shutdown"]"#).unwrap();
        assert_eq!(c, [Criterion::CommonCause, Criterion::TsShutdown]);
        let r: Vec<ReactorType> = serde_json::from_str(r#"["pwr", "BWR"]"#).unwrap();
        assert_eq!(r, [ReactorType::Pwr, ReactorType::Bwr]);
        assert!(serde_json::from_str::<Cause>(r#""human_error""#).is_err());
        for c in Criterion::ALL {
            let back: Criterion = serde_json::from_value(c.as_str().into()).unwrap();
            assert_eq!(back, *c);
        }
    }

    #[test]
    fn emergency_declared_sections() {
        let e = Criterion::EmergencyDeclared;
        for yes in [
            "50.72(a)(1)(i)",
            "50.72(a)(1)",
            "50.72(a)(i)(i)",
            "50.72(1)(i)",
            "50.72(a)(3)",
        ] {
            assert!(e.matches_section(yes), "{yes}");
        }
        for no in [
            "50.72(a)",
            "50.72(a)(2)(i)(b)",
            "50.72(a)(2)(iv)",
            "50.72(a)(10)",
            "50.72(b)(3)(iv)(a)",
        ] {
            assert!(!e.matches_section(no), "{no}");
        }
        assert!(Criterion::SafetyFunctionFailure.matches_section("50.73(a)(2)(v)(d)"));
        assert!(!Criterion::SafetyFunctionFailure.matches_section("50.73(a)(2)(vii)"));
    }
}
