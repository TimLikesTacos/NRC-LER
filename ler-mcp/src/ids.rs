//! Identifiers a user (or the model) can pass to look up an event, parsed and validated before
//! any SQL runs. Each type's `ToSql` produces exactly the value stored in the column it's matched
//! against, so the queries in db.rs read as `WHERE ler_key = ?1` with a typed parameter.

use std::fmt;
use std::str::FromStr;

use rusqlite::ToSql;
use rusqlite::types::ToSqlOutput;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdError {
    #[error("empty identifier")]
    Empty,
    #[error(
        "{0:?} is not a recognized LER or accession number. Accepted: \"327-2014-002\", \
         \"05000327/2014-002-01\", \"50-327-2014-002\", \"2014-002\" or \"14-002-00\" (any unit), \
         or an ADAMS accession like \"ML14219A568\""
    )]
    Unrecognized(String),
    #[error(
        "{0:?} is not an ADAMS accession number (expected ML + 9 characters, like ML14219A568)"
    )]
    Accession(String),
    #[error(
        "{0:?} is not a docket (expected 1-5 digits like 327, or 8 digits like 05000327 or 05200025)"
    )]
    Docket(String),
    #[error("{0:?} is not an LER year (expected 2 or 4 digits, 1969-2099)")]
    Year(String),
    #[error("{0:?} is not an LER sequence number (expected 3 digits, 001-999)")]
    Sequence(String),
    #[error("{0:?} is not an LER revision (expected 1 or 2 digits)")]
    Revision(String),
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------- Docket

/// An NRC docket: "050" (10 CFR Part 50 license) or "052" (Part 52) plus a 5-digit number.
/// Stored in the database as 8 digits, e.g. "05000327".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Docket {
    part: u8, // 50 or 52
    number: u32,
}

impl Docket {
    fn new(part: u8, number: u32) -> Option<Self> {
        (matches!(part, 50 | 52) && (1..=99_999).contains(&number)).then_some(Self { part, number })
    }

    /// From one token ("327", "05000327") or two ("50", "327").
    fn from_tokens(tokens: &[&str]) -> Result<Self, IdError> {
        let err = || IdError::Docket(tokens.join("-"));
        let (part, number) = match tokens {
            [n] if all_digits(n) && n.len() <= 5 => (50, n.parse().map_err(|_| err())?),
            [n] if all_digits(n) && n.len() == 8 => (
                n[..3].parse().map_err(|_| err())?,
                n[3..].parse().map_err(|_| err())?,
            ),
            [p, n] if all_digits(p) && all_digits(n) && p.len() <= 3 && n.len() <= 5 => {
                (p.parse().map_err(|_| err())?, n.parse().map_err(|_| err())?)
            }
            _ => return Err(err()),
        };
        Self::new(part, number).ok_or_else(err)
    }
}

impl FromStr for Docket {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        let tokens: Vec<&str> = s
            .trim()
            .split(['-', '/', ' '])
            .filter(|t| !t.is_empty())
            .collect();
        Self::from_tokens(&tokens)
    }
}

impl fmt::Display for Docket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:03}{:05}", self.part, self.number)
    }
}

/// Matches `lers.docket` / `units.docket`, e.g. "05000327".
impl ToSql for Docket {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.to_string()))
    }
}

// ---------------------------------------------------------------- LER numbers

/// The year-sequence part of an LER number ("2014-002"), with an optional revision ("-01").
/// Sequence numbers restart per docket each year, so without a docket this matches every plant
/// that filed that number; it's looked up as "any unit".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LerSequence {
    pub year: u16,
    pub seq: u16,
    pub revision: Option<u8>,
}

impl LerSequence {
    fn from_tokens(year: &str, seq: &str, revision: Option<&str>) -> Result<Self, IdError> {
        let year = match (all_digits(year), year.len()) {
            (true, 4) => year.parse().ok().filter(|y| (1969..=2099).contains(y)),
            // Two-digit years as printed on older forms ("14-002-00"); the LER program began in 1969.
            (true, 2) => year
                .parse::<u16>()
                .ok()
                .map(|y| if y >= 69 { 1900 + y } else { 2000 + y }),
            _ => None,
        }
        .ok_or_else(|| IdError::Year(year.to_string()))?;
        let seq = Some(seq)
            .filter(|s| all_digits(s) && s.len() == 3)
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .ok_or_else(|| IdError::Sequence(seq.to_string()))?;
        let revision = revision
            .map(|r| {
                Some(r)
                    .filter(|r| all_digits(r) && r.len() <= 2)
                    .and_then(|r| r.parse().ok())
                    .ok_or_else(|| IdError::Revision(r.to_string()))
            })
            .transpose()?;
        Ok(Self {
            year,
            seq,
            revision,
        })
    }
}

impl fmt::Display for LerSequence {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}-{:03}", self.year, self.seq)?;
        match self.revision {
            Some(r) => write!(f, "-{r:02}"),
            None => Ok(()),
        }
    }
}

/// Matches the first 8 characters of `lers.ler_number` ("2014-002" of "2014-002-00");
/// the revision is a separate parameter (`lers.ler_rev`).
impl ToSql for LerSequence {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(format!("{}-{:03}", self.year, self.seq)))
    }
}

/// A full LER number: docket + year + sequence (+ revision), e.g. "05000327/2014-002-00".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LerNumber {
    pub docket: Docket,
    pub sequence: LerSequence,
}

impl fmt::Display for LerNumber {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}", self.docket, self.sequence)
    }
}

/// Matches `lers.ler_key`, e.g. "05000327:2014-002" (all revisions of one report share it).
impl ToSql for LerNumber {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(format!(
            "{}:{}-{:03}",
            self.docket, self.sequence.year, self.sequence.seq
        )))
    }
}

/// Either kind of LER reference, depending on whether a docket was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LerRef {
    Full(LerNumber),
    AnyUnit(LerSequence),
}

impl FromStr for LerRef {
    type Err = IdError;

    /// Splits on '-', '/', ':' or spaces, then reads the tokens right to left:
    /// optional revision (1-2 digits after a 3-digit sequence), sequence, year, optional docket.
    fn from_str(s: &str) -> Result<Self, IdError> {
        let unrecognized = || IdError::Unrecognized(s.to_string());
        let mut tokens: Vec<&str> = s
            .split(['-', '/', ':', ' '])
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.len() < 2 || !tokens.iter().all(|t| all_digits(t)) {
            return Err(unrecognized());
        }
        let n = tokens.len();
        let revision = (n >= 3 && tokens[n - 1].len() <= 2 && tokens[n - 2].len() == 3)
            .then(|| tokens.pop().unwrap());
        let (seq, year) = (
            tokens.pop().unwrap(),
            tokens.pop().ok_or_else(unrecognized)?,
        );
        let sequence = LerSequence::from_tokens(year, seq, revision)?;
        Ok(match tokens.as_slice() {
            [] => LerRef::AnyUnit(sequence),
            docket => LerRef::Full(LerNumber {
                docket: Docket::from_tokens(docket)?,
                sequence,
            }),
        })
    }
}

// ---------------------------------------------------------------- ADAMS accession

/// An ADAMS accession number: "ML" + 9 characters, e.g. ML14219A568 or ML091870422.
/// The 8th character may be a letter or digit; all others are digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accession(String);

impl FromStr for Accession {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        let up = s.trim().to_ascii_uppercase();
        let b = up.as_bytes();
        let ok = b.len() == 11
            && b.starts_with(b"ML")
            && b[2..7].iter().all(u8::is_ascii_digit)
            && (b[7].is_ascii_digit() || b[7].is_ascii_uppercase())
            && b[8..].iter().all(u8::is_ascii_digit);
        if ok {
            Ok(Self(up))
        } else {
            Err(IdError::Accession(s.to_string()))
        }
    }
}

impl fmt::Display for Accession {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Matches `lers.accession_number`.
impl ToSql for Accession {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.as_str()))
    }
}

// ---------------------------------------------------------------- EventId

/// What `get_event` accepts: an accession or an LER reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventId {
    Accession(Accession),
    Ler(LerRef),
}

impl FromStr for EventId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(IdError::Empty);
        }
        if s.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("ML")) {
            return s.parse().map(EventId::Accession);
        }
        // Allow a leading "LER" / "LER#" as people write it in titles.
        let s = match s.get(..3) {
            Some(p) if p.eq_ignore_ascii_case("LER") => s[3..].trim_start_matches(['#', ' ']),
            _ => s,
        };
        s.parse().map(EventId::Ler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::types::{ToSqlOutput, Value, ValueRef};

    /// The text a ToSql impl hands to SQLite, whether borrowed or owned.
    fn sql_text(v: &dyn ToSql) -> String {
        match v.to_sql().unwrap() {
            ToSqlOutput::Borrowed(ValueRef::Text(t)) => String::from_utf8(t.to_vec()).unwrap(),
            ToSqlOutput::Owned(Value::Text(t)) => t,
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn seq(year: u16, seq: u16, revision: Option<u8>) -> LerSequence {
        LerSequence {
            year,
            seq,
            revision,
        }
    }

    fn full(part: u8, number: u32, s: LerSequence) -> EventId {
        EventId::Ler(LerRef::Full(LerNumber {
            docket: Docket::new(part, number).unwrap(),
            sequence: s,
        }))
    }

    // --- Docket
    #[test]
    fn docket_forms() {
        for s in ["327", "00327", "05000327", "50-327", "050-00327", " 327 "] {
            assert_eq!(s.parse::<Docket>().unwrap().to_string(), "05000327", "{s}");
        }
        assert_eq!("52-25".parse::<Docket>().unwrap().to_string(), "05200025");
        assert_eq!(
            "05200025".parse::<Docket>().unwrap().to_string(),
            "05200025"
        );
    }

    #[test]
    fn docket_rejects() {
        for s in [
            "", "0", "00000", "123456", "05100327", "51-327", "05000000", "abc", "1234567",
            "50-327-1",
        ] {
            assert!(
                matches!(s.parse::<Docket>(), Err(IdError::Docket(_))),
                "{s:?}"
            );
        }
    }

    #[test]
    fn docket_to_sql() {
        assert_eq!(sql_text(&"328".parse::<Docket>().unwrap()), "05000328");
    }

    // --- LER numbers
    #[test]
    fn ler_full_forms() {
        let want = full(50, 327, seq(2014, 2, None));
        for s in [
            "327-2014-002",
            "05000327-2014-002",
            "05000327/2014-002",
            "05000327:2014-002",
            "50-327-2014-002",
            "050-00327/2014-002",
            "327 2014 002",
            "LER 327-2014-002",
            "ler#327-2014-002",
        ] {
            assert_eq!(s.parse::<EventId>().unwrap(), want, "{s}");
        }
        assert_eq!("327-14-002".parse::<EventId>().unwrap(), want);
    }

    #[test]
    fn ler_with_revision() {
        let want = full(50, 327, seq(2014, 2, Some(1)));
        for s in [
            "327-2014-002-01",
            "05000327/2014-002-01",
            "50-327-2014-002-1",
            "327-14-002-01",
        ] {
            assert_eq!(s.parse::<EventId>().unwrap(), want, "{s}");
        }
        assert_eq!(
            "52-25-2020-001-00".parse::<EventId>().unwrap(),
            full(52, 25, seq(2020, 1, Some(0)))
        );
    }

    #[test]
    fn ler_any_unit() {
        let any = |s| EventId::Ler(LerRef::AnyUnit(s));
        assert_eq!(
            "2014-002".parse::<EventId>().unwrap(),
            any(seq(2014, 2, None))
        );
        assert_eq!(
            "2014-002-00".parse::<EventId>().unwrap(),
            any(seq(2014, 2, Some(0)))
        );
        assert_eq!(
            "LER 14-002-00".parse::<EventId>().unwrap(),
            any(seq(2014, 2, Some(0)))
        );
        assert_eq!(
            "85-010".parse::<EventId>().unwrap(),
            any(seq(1985, 10, None))
        );
        assert_eq!(
            "69-001".parse::<EventId>().unwrap(),
            any(seq(1969, 1, None))
        );
        assert_eq!(
            "68-001".parse::<EventId>().unwrap(),
            any(seq(2068, 1, None))
        );
    }

    #[test]
    fn ler_rejects() {
        type IsExpected = fn(&IdError) -> bool;
        let cases: &[(&str, IsExpected)] = &[
            ("", |e| *e == IdError::Empty),
            ("   ", |e| *e == IdError::Empty),
            ("2014", |e| matches!(e, IdError::Unrecognized(_))),
            ("sequoyah 2014-002", |e| {
                matches!(e, IdError::Unrecognized(_))
            }),
            ("327-2014-0002", |e| matches!(e, IdError::Sequence(_))),
            ("327-2014-000", |e| matches!(e, IdError::Sequence(_))),
            ("327-2014-02", |e| matches!(e, IdError::Sequence(_))),
            ("327-1875-002", |e| matches!(e, IdError::Year(_))),
            ("327-201-002", |e| matches!(e, IdError::Year(_))),
            ("999999-2014-002", |e| matches!(e, IdError::Docket(_))),
            ("51-327-2014-002", |e| matches!(e, IdError::Docket(_))),
            ("1-2-327-2014-002", |e| matches!(e, IdError::Docket(_))),
        ];
        for (s, is_expected) in cases {
            let err = s.parse::<EventId>().unwrap_err();
            assert!(is_expected(&err), "{s:?} gave {err:?}");
        }
    }

    #[test]
    fn ler_display_round_trip() {
        let EventId::Ler(LerRef::Full(n)) = "327-14-002-1".parse().unwrap() else {
            panic!()
        };
        assert_eq!(n.to_string(), "05000327/2014-002-01");
        assert_eq!(
            n.to_string().parse::<EventId>().unwrap(),
            EventId::Ler(LerRef::Full(n))
        );
    }

    #[test]
    fn ler_to_sql() {
        let EventId::Ler(LerRef::Full(n)) = "327-2014-002-01".parse().unwrap() else {
            panic!()
        };
        assert_eq!(sql_text(&n), "05000327:2014-002", "ler_key has no revision");
        assert_eq!(sql_text(&n.sequence), "2014-002");
        assert_eq!(sql_text(&seq(1999, 45, Some(3))), "1999-045");
    }

    // --- Accession
    #[test]
    fn accession_forms() {
        for (s, want) in [
            ("ML14219A568", "ML14219A568"),
            ("ml14219a568", "ML14219A568"),
            (" ML091870422 ", "ML091870422"),
        ] {
            let EventId::Accession(a) = s.parse().unwrap() else {
                panic!("{s}")
            };
            assert_eq!(a.to_string(), want);
            assert_eq!(sql_text(&a), want);
        }
    }

    #[test]
    fn accession_rejects() {
        for s in [
            "ML1421",
            "ML14219A5689",
            "ML14219AA68",
            "MLA4219A568",
            "ML14219A56X",
            "XL14219A568",
        ] {
            let r = s.parse::<Accession>();
            assert!(matches!(r, Err(IdError::Accession(_))), "{s:?} gave {r:?}");
        }
        assert!(matches!(
            "ML14219AA68".parse::<EventId>(),
            Err(IdError::Accession(_))
        ));
    }
}
