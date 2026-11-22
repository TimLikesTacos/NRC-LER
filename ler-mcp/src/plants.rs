//! Resolving what people type for a plant to dockets: "Sequoyah", "SQN", "seqouah",
//! "Sequoyah Nuclear Plant", "Palo Verde Unit 2", "Fermi 2", "327", "05000327".
//!
//! Loaded once from the sites / site_aliases / units tables (reference data, ~85 sites).

use rusqlite::Connection;

#[derive(Debug, Clone)]
struct Site {
    id: String,
    name: String,
    /// Normalized name (see `norm`).
    key: String,
    /// Normalized aliases ("sqn", "pvngs").
    aliases: Vec<String>,
}

#[derive(Debug, Clone)]
struct Unit {
    docket: String,
    site_id: String,
    /// "1", "2"; absent for single-unit sites.
    unit: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PlantIndex {
    sites: Vec<Site>,
    units: Vec<Unit>,
}

/// The dockets a plant filter stands for, and a line saying how it was read.
#[derive(Debug, Clone, PartialEq)]
pub struct PlantMatch {
    pub dockets: Vec<String>,
    pub note: String,
}

/// Words that say "a plant" without naming one.
const NOISE: &[&str] = &[
    "nuclear",
    "power",
    "plant",
    "plants",
    "station",
    "generating",
    "electric",
    "steam",
    "npp",
    "ngs",
    "nps",
    "the",
    "site",
    "facility",
    "reactor",
    "units",
];

/// Lowercase letters and digits only: "St. Lucie" and "st lucie" -> "stlucie".
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Optimal string alignment distance (Levenshtein plus adjacent transpositions): "seqouah" is
/// 2 from "sequoyah".
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// "327", "50-327", "05000327" -> "05000327".
fn as_docket(input: &str) -> Option<String> {
    let digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
    let only_docket_chars = input
        .trim()
        .chars()
        .all(|c| c.is_ascii_digit() || c == '-' || c == '/');
    if !only_docket_chars {
        return None;
    }
    match digits.len() {
        3 => Some(format!("05000{digits}")),
        5 if digits.starts_with("50") => Some(format!("05000{}", &digits[2..])),
        8 if digits.starts_with("05000") => Some(digits),
        _ => None,
    }
}

/// Splits "Palo Verde Unit 2" / "Fermi 2" / "SQN U1" into the name key and a unit number.
fn split_unit(input: &str) -> (String, Option<String>) {
    let tokens: Vec<String> = input
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut unit = None;
    let mut name = String::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        let single_digit = |s: &str| s.len() == 1 && s.chars().all(|c| c.is_ascii_digit());
        if t == "unit" && i + 1 < tokens.len() && single_digit(&tokens[i + 1]) {
            unit = Some(tokens[i + 1].clone());
            i += 2;
            continue;
        }
        if let Some(n) = t.strip_prefix("unit").or_else(|| t.strip_prefix('u'))
            && single_digit(n)
        {
            unit = Some(n.to_string());
        } else if single_digit(t) && !name.is_empty() {
            unit = Some(t.to_string());
        } else if !NOISE.contains(&t) && t != "unit" {
            name.push_str(t);
        }
        i += 1;
    }
    (name, unit)
}

impl PlantIndex {
    pub fn load(db: &Connection) -> rusqlite::Result<Self> {
        let has_aliases: bool = db.query_row(
            "SELECT count(*) > 0 FROM sqlite_master WHERE name = 'site_aliases'",
            [],
            |r| r.get(0),
        )?;
        let alias_sql = if has_aliases {
            "SELECT site_id, alias FROM site_aliases"
        } else {
            "SELECT NULL, NULL WHERE 0"
        };
        let aliases: Vec<(String, String)> = db
            .prepare(alias_sql)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let sites = db
            .prepare("SELECT site_id, name FROM sites ORDER BY site_id")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .map(|row| {
                row.map(|(id, name)| Site {
                    aliases: aliases
                        .iter()
                        .filter(|(s, _)| *s == id)
                        .map(|(_, a)| norm(a))
                        .collect(),
                    key: norm(&name),
                    id,
                    name,
                })
            })
            .collect::<rusqlite::Result<_>>()?;
        let units = db
            .prepare("SELECT docket, site_id, NULLIF(unit, '') FROM units WHERE site_id IS NOT NULL ORDER BY docket")?
            .query_map([], |r| {
                Ok(Unit {
                    docket: r.get(0)?,
                    site_id: r.get(1)?,
                    unit: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Self { sites, units })
    }

    pub fn resolve(&self, input: &str) -> Result<PlantMatch, String> {
        if let Some(d) = as_docket(input) {
            return match self.units.iter().find(|u| u.docket == d) {
                Some(u) => Ok(PlantMatch {
                    note: format!("Plant: docket {d} ({})", self.unit_label(u)),
                    dockets: vec![d],
                }),
                None => Err(format!("no unit has docket {d}")),
            };
        }
        // Whole-input aliases first: "WNP-2", "IP2", "CR3" carry a digit that isn't a unit.
        let whole = norm(input);
        let mut sites: Vec<&Site> = self
            .sites
            .iter()
            .filter(|s| s.aliases.contains(&whole))
            .collect();
        let mut unit = None;
        let mut corrected = None;
        if sites.is_empty() {
            let (key, u) = split_unit(input);
            unit = u;
            if key.is_empty() {
                return Err(format!("{input:?} doesn't name a plant"));
            }
            sites = self.name_matches(&key);
            if sites.is_empty() {
                match self.closest(&key) {
                    Ok(s) => {
                        corrected = Some(s.name.clone());
                        sites = vec![s];
                    }
                    Err(suggestions) => {
                        return Err(format!(
                            "no plant matches {input:?}. Closest: {}",
                            suggestions.join(", ")
                        ));
                    }
                }
            }
        }
        self.dockets(input, &sites, unit.as_deref(), corrected)
    }

    /// Sites whose alias is `key`, or whose name equals, starts with, or contains it (or that
    /// `key` contains, for inputs with extra words).
    fn name_matches(&self, key: &str) -> Vec<&Site> {
        let exact: Vec<&Site> = self
            .sites
            .iter()
            .filter(|s| s.key == key || s.aliases.iter().any(|a| a == key))
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        self.sites
            .iter()
            .filter(|s| {
                (key.len() >= 3 && s.key.starts_with(key))
                    || (key.len() >= 4 && s.key.contains(key))
                    || (s.key.len() >= 4 && key.contains(&s.key))
            })
            .collect()
    }

    /// The one site clearly closest to `key` by spelling, or the three closest as suggestions.
    fn closest(&self, key: &str) -> Result<&Site, Vec<String>> {
        let mut scored: Vec<(usize, &Site)> = self
            .sites
            .iter()
            .map(|s| {
                let by_alias = s
                    .aliases
                    .iter()
                    .filter(|a| a.len() >= 4)
                    .map(|a| distance(key, a));
                let best = by_alias
                    .chain([distance(key, &s.key)])
                    .min()
                    .unwrap_or(usize::MAX);
                (best, s)
            })
            .collect();
        scored.sort_by_key(|(d, s)| (*d, s.id.clone()));
        let allowed = if key.len() >= 7 {
            2
        } else if key.len() >= 5 {
            1
        } else {
            0
        };
        match scored.as_slice() {
            [(d, s), rest @ ..] if *d <= allowed && rest.first().is_none_or(|(d2, _)| d2 > d) => {
                Ok(s)
            }
            _ => Err(scored.iter().take(3).map(|(_, s)| s.name.clone()).collect()),
        }
    }

    fn dockets(
        &self,
        input: &str,
        sites: &[&Site],
        unit: Option<&str>,
        corrected: Option<String>,
    ) -> Result<PlantMatch, String> {
        let at_sites: Vec<&Unit> = self
            .units
            .iter()
            .filter(|u| sites.iter().any(|s| s.id == u.site_id))
            .collect();
        // A unit number only narrows multi-unit sites ("Waterford 3" is the whole site).
        let numbered = at_sites.iter().any(|u| u.unit.is_some());
        let chosen: Vec<&Unit> = match unit {
            Some(n) if numbered => at_sites
                .iter()
                .copied()
                .filter(|u| u.unit.as_deref() == Some(n))
                .collect(),
            _ => at_sites.clone(),
        };
        if chosen.is_empty() {
            let have: Vec<&str> = at_sites.iter().filter_map(|u| u.unit.as_deref()).collect();
            return Err(format!(
                "{} has no unit {}; units: {}",
                sites
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                unit.unwrap_or("?"),
                have.join(", ")
            ));
        }
        let names: Vec<String> = chosen.iter().map(|u| self.unit_label(u)).collect();
        let mut note = format!("Plant: {}", names.join(", "));
        if let Some(name) = corrected {
            note = format!("No plant is named {input:?}; using the closest match, {name}. {note}");
        } else if sites.len() > 1 {
            note = format!("{input:?} matches {} plants. {note}", sites.len());
        }
        Ok(PlantMatch {
            dockets: chosen.iter().map(|u| u.docket.clone()).collect(),
            note,
        })
    }

    fn unit_label(&self, u: &Unit) -> String {
        let site = self
            .sites
            .iter()
            .find(|s| s.id == u.site_id)
            .map_or("?", |s| s.name.as_str());
        match &u.unit {
            Some(n) => format!("{site} Unit {n}"),
            None => site.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> PlantIndex {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE sites (site_id TEXT PRIMARY KEY, name TEXT, state TEXT);
             CREATE TABLE site_aliases (site_id TEXT, alias TEXT);
             CREATE TABLE units (docket TEXT PRIMARY KEY, site_id TEXT, unit TEXT, name TEXT);
             INSERT INTO sites VALUES ('sequoyah', 'Sequoyah', 'TN'), ('palo-verde', 'Palo Verde', 'AZ'),
               ('columbia', 'Columbia Generating Station', 'WA'), ('st-lucie', 'St. Lucie', 'FL'),
               ('maine-yankee', 'Maine Yankee', 'ME'), ('vermont-yankee', 'Vermont Yankee', 'VT'),
               ('waterford', 'Waterford', 'LA');
             INSERT INTO site_aliases VALUES ('sequoyah', 'SQN'), ('columbia', 'WNP-2'), ('palo-verde', 'PVNGS');
             INSERT INTO units VALUES ('05000327', 'sequoyah', '1', ''), ('05000328', 'sequoyah', '2', ''),
               ('05000528', 'palo-verde', '1', ''), ('05000529', 'palo-verde', '2', ''),
               ('05000530', 'palo-verde', '3', ''), ('05000397', 'columbia', NULL, ''),
               ('05000335', 'st-lucie', '1', ''), ('05000389', 'st-lucie', '2', ''),
               ('05000309', 'maine-yankee', NULL, ''), ('05000271', 'vermont-yankee', NULL, ''),
               ('05000382', 'waterford', '3', '');",
        )
        .unwrap();
        PlantIndex::load(&db).unwrap()
    }

    fn dockets(input: &str) -> Vec<String> {
        index()
            .resolve(input)
            .unwrap_or_else(|e| panic!("{input}: {e}"))
            .dockets
    }

    #[test]
    fn names_aliases_and_noise_words() {
        let sqn = ["05000327", "05000328"];
        for input in [
            "Sequoyah",
            "sequoyah nuclear plant",
            "SQN",
            "Sequoyah Nuclear Power Plant",
            "seq",
        ] {
            assert_eq!(dockets(input), sqn, "{input}");
        }
        assert_eq!(dockets("st lucie"), ["05000335", "05000389"]);
        assert_eq!(dockets("Columbia"), ["05000397"]);
        assert_eq!(
            dockets("WNP-2"),
            ["05000397"],
            "digit in an alias isn't a unit"
        );
    }

    #[test]
    fn units_and_dockets() {
        assert_eq!(dockets("Palo Verde Unit 2"), ["05000529"]);
        assert_eq!(dockets("PVNGS U3"), ["05000530"]);
        assert_eq!(dockets("St. Lucie 1"), ["05000335"]);
        assert_eq!(dockets("Waterford 3"), ["05000382"]);
        for d in ["327", "50-327", "05000327"] {
            assert_eq!(dockets(d), ["05000327"], "{d}");
        }
        let e = index().resolve("Palo Verde Unit 5").unwrap_err();
        assert!(e.contains("units: 1, 2, 3"), "{e}");
    }

    #[test]
    fn misspellings_corrected_or_suggested() {
        let m = index().resolve("seqouah").unwrap();
        assert_eq!(m.dockets, ["05000327", "05000328"]);
        assert!(
            m.note
                .starts_with("No plant is named \"seqouah\"; using the closest match, Sequoyah."),
            "{}",
            m.note
        );
        let e = index().resolve("zzqqxx").unwrap_err();
        assert!(
            e.starts_with("no plant matches \"zzqqxx\". Closest: "),
            "{e}"
        );
    }

    #[test]
    fn several_sites_named_together() {
        let m = index().resolve("yankee").unwrap();
        assert_eq!(m.dockets, ["05000271", "05000309"]);
        assert!(
            m.note.starts_with("\"yankee\" matches 2 plants."),
            "{}",
            m.note
        );
    }

    #[test]
    fn distance_counts_transpositions() {
        assert_eq!(distance("seqouah", "sequoyah"), 2);
        assert_eq!(distance("abc", "abc"), 0);
        assert_eq!(distance("ab", "ba"), 1);
    }
}
