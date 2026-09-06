use serde::{Deserialize, Serialize};

use crate::local::LocalBeatmap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operator {
    Eq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
}

impl Operator {
    pub const ALL: [Self; 6] = [
        Self::Eq,
        Self::NotEq,
        Self::Lt,
        Self::Gt,
        Self::Le,
        Self::Ge,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Le => "<=",
            Self::Ge => ">=",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SearchField {
    FreeText,
    Artist,
    Title,
    Source,
    Favourites,
    FeaturedArtist,
    Creator,
    Difficulty,
    ApproachRate,
    CircleSize,
    OverallDifficulty,
    HpDrain,
    StarRating,
    Bpm,
    Length,
    Divisor,
    Circles,
    Sliders,
    Keys,
    Status,
    Created,
    Submitted,
    Updated,
    Ranked,
    Tag,
    Mode,
}

impl SearchField {
    pub const SORTED: [Self; 16] = [
        Self::ApproachRate,
        Self::Artist,
        Self::Bpm,
        Self::CircleSize,
        Self::Creator,
        Self::HpDrain,
        Self::Length,
        Self::Mode,
        Self::OverallDifficulty,
        Self::Ranked,
        Self::Created,
        Self::StarRating,
        Self::Status,
        Self::Tag,
        Self::Title,
        Self::Updated,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::FreeText => "Text",
            Self::Artist => "Artist",
            Self::Title => "Title",
            Self::Source => "Source",
            Self::Favourites => "Favourites",
            Self::FeaturedArtist => "Featured artist",
            Self::Creator => "Mapper",
            Self::Difficulty => "Difficulty name",
            Self::ApproachRate => "AR",
            Self::CircleSize => "CS",
            Self::OverallDifficulty => "OD",
            Self::HpDrain => "HP/DR",
            Self::StarRating => "Stars",
            Self::Bpm => "BPM",
            Self::Length => "Length",
            Self::Divisor => "Divisor",
            Self::Circles => "Circles",
            Self::Sliders => "Sliders",
            Self::Keys => "Keys",
            Self::Status => "Status",
            Self::Created => "Created",
            Self::Submitted => "Submitted",
            Self::Updated => "Updated",
            Self::Ranked => "Ranked date",
            Self::Tag => "User tag",
            Self::Mode => "Mode",
        }
    }

    pub fn key(self) -> Option<&'static str> {
        match self {
            Self::FreeText => None,
            Self::Artist => Some("artist"),
            Self::Title => Some("title"),
            Self::Source => Some("source"),
            Self::Favourites => Some("favourites"),
            Self::FeaturedArtist => Some("featured_artist"),
            Self::Creator => Some("creator"),
            Self::Difficulty => Some("difficulty"),
            Self::ApproachRate => Some("ar"),
            Self::CircleSize => Some("cs"),
            Self::OverallDifficulty => Some("od"),
            Self::HpDrain => Some("hp"),
            Self::StarRating => Some("stars"),
            Self::Bpm => Some("bpm"),
            Self::Length => Some("length"),
            Self::Divisor => Some("divisor"),
            Self::Circles => Some("circles"),
            Self::Sliders => Some("sliders"),
            Self::Keys => Some("keys"),
            Self::Status => Some("status"),
            Self::Created => Some("created"),
            Self::Submitted => Some("submitted"),
            Self::Updated => Some("updated"),
            Self::Ranked => Some("ranked"),
            Self::Tag => Some("tag"),
            Self::Mode => Some("mode"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryClause {
    pub field: SearchField,
    pub operator: Operator,
    pub value: String,
    pub enabled: bool,
}

impl Default for QueryClause {
    fn default() -> Self {
        Self {
            field: SearchField::Artist,
            operator: Operator::Eq,
            value: String::new(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BeatmapQuery {
    pub clauses: Vec<QueryClause>,
}

impl BeatmapQuery {
    pub fn to_osu_search(&self) -> String {
        self.clauses
            .iter()
            .filter(|clause| clause.enabled && !clause.value.trim().is_empty())
            .map(QueryClause::to_token)
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn matches_local(&self, map: &LocalBeatmap) -> bool {
        self.clauses
            .iter()
            .filter(|clause| clause.enabled && !clause.value.trim().is_empty())
            .all(|clause| clause.matches_local(map))
    }
}

impl QueryClause {
    pub fn to_token(&self) -> String {
        let value = quote_if_needed(self.value.trim());
        match self.field.key() {
            Some(key) => format!("{}{}{}", key, self.operator.as_str(), value),
            None => value,
        }
    }

    pub fn matches_local(&self, map: &LocalBeatmap) -> bool {
        let needle = self.value.trim();
        match self.field {
            SearchField::FreeText => compare_text(
                &[
                    map.artist.as_str(),
                    map.title.as_str(),
                    map.source.as_str(),
                    map.creator.as_str(),
                    map.version.as_str(),
                    map.tags.as_str(),
                ]
                .join(" "),
                needle,
                self.operator,
            ),
            SearchField::Artist => compare_text(&map.artist, needle, self.operator),
            SearchField::Title => compare_text(&map.title, needle, self.operator),
            SearchField::Source => compare_text(&map.source, needle, self.operator),
            SearchField::Creator => compare_text(&map.creator, needle, self.operator),
            SearchField::Difficulty => compare_text(&map.version, needle, self.operator),
            SearchField::Tag => compare_text(&map.tags, needle, self.operator),
            SearchField::ApproachRate => compare_optional_number(map.ar, needle, self.operator),
            SearchField::CircleSize => compare_optional_number(map.cs, needle, self.operator),
            SearchField::OverallDifficulty => {
                compare_optional_number(map.od, needle, self.operator)
            }
            SearchField::HpDrain => compare_optional_number(map.hp, needle, self.operator),
            SearchField::StarRating => compare_optional_number(map.stars, needle, self.operator),
            SearchField::Bpm => compare_optional_number(map.bpm, needle, self.operator),
            SearchField::Length => {
                compare_optional_number(map.length_seconds, needle, self.operator)
            }
            SearchField::Circles => compare_number(map.circles as f32, needle, self.operator),
            SearchField::Sliders => compare_number(map.sliders as f32, needle, self.operator),
            SearchField::Keys => compare_optional_number(map.cs, needle, self.operator),
            SearchField::Mode => compare_text(mode_label(map.mode), needle, self.operator),
            SearchField::Favourites
            | SearchField::FeaturedArtist
            | SearchField::Divisor
            | SearchField::Status
            | SearchField::Created
            | SearchField::Submitted
            | SearchField::Updated
            | SearchField::Ranked => false,
        }
    }
}

fn quote_if_needed(value: &str) -> String {
    if value.contains(' ') || value.contains('/') || value.contains('"') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

fn compare_text(haystack: &str, needle: &str, operator: Operator) -> bool {
    let haystack = haystack.to_lowercase();
    let needles = needle
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);

    match operator {
        Operator::Eq => needles.into_iter().any(|needle| haystack.contains(&needle)),
        Operator::NotEq => needles
            .into_iter()
            .all(|needle| !haystack.contains(&needle)),
        Operator::Lt | Operator::Gt | Operator::Le | Operator::Ge => false,
    }
}

fn compare_optional_number(value: Option<f32>, needle: &str, operator: Operator) -> bool {
    value.is_some_and(|value| compare_number(value, needle, operator))
}

fn compare_number(value: f32, needle: &str, operator: Operator) -> bool {
    let Ok(expected) = needle.parse::<f32>() else {
        return false;
    };

    match operator {
        Operator::Eq => (value - expected).abs() < f32::EPSILON,
        Operator::NotEq => (value - expected).abs() >= f32::EPSILON,
        Operator::Lt => value < expected,
        Operator::Gt => value > expected,
        Operator::Le => value <= expected,
        Operator::Ge => value >= expected,
    }
}

fn mode_label(mode: Option<u8>) -> &'static str {
    // A missing Mode field means osu!std in the .osu format.
    match mode {
        None | Some(0) => "osu",
        Some(1) => "taiko",
        Some(2) => "catch ctb fruits",
        Some(3) => "mania",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_pairable_osu_query() {
        let query = BeatmapQuery {
            clauses: vec![
                QueryClause {
                    field: SearchField::Artist,
                    operator: Operator::Eq,
                    value: "Camellia".into(),
                    enabled: true,
                },
                QueryClause {
                    field: SearchField::StarRating,
                    operator: Operator::Ge,
                    value: "5.5".into(),
                    enabled: true,
                },
                QueryClause {
                    field: SearchField::Status,
                    operator: Operator::Eq,
                    value: "ranked,loved".into(),
                    enabled: true,
                },
            ],
        };

        assert_eq!(
            query.to_osu_search(),
            "artist=Camellia stars>=5.5 status=ranked,loved"
        );
    }
}
