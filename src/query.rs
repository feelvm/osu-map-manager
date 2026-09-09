//! Beginner-friendly collection filters.
//!
//! Instead of free-form `field<operator>value` rows, the UI offers one
//! control per idea: text boxes for words (artist, title, mapper, ...),
//! min/max sliders for numbers (stars, AR, ...), text boxes for the song
//! length, and a dropdown for the game mode.

use serde::{Deserialize, Serialize};

use crate::local::LocalBeatmap;

/// Full slider bounds for every numeric filter, shared by the UI and matching.
pub const STARS_RANGE: (f32, f32) = (0.0, 12.0);
pub const AR_RANGE: (f32, f32) = (0.0, 11.0);
pub const CS_RANGE: (f32, f32) = (0.0, 10.0);
pub const OD_RANGE: (f32, f32) = (0.0, 11.0);
pub const HP_RANGE: (f32, f32) = (0.0, 10.0);
pub const BPM_RANGE: (f32, f32) = (0.0, 350.0);

/// Closed numeric range picked with min/max sliders. Disabled means "any".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RangeFilter {
    pub enabled: bool,
    pub min: f32,
    pub max: f32,
}

impl RangeFilter {
    pub const fn new(min: f32, max: f32) -> Self {
        Self {
            enabled: false,
            min,
            max,
        }
    }

    pub fn matches(&self, value: Option<f32>) -> bool {
        if !self.enabled {
            return true;
        }
        value.is_some_and(|value| value >= self.min && value <= self.max)
    }

    /// osu!web-style tokens (`stars>=6 stars<=8`). Only emitted when enabled.
    fn tokens(&self, key: &str, full: (f32, f32)) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        let mut tokens = Vec::new();
        if self.min > full.0 {
            tokens.push(format!("{key}>={}", trim_number(self.min)));
        }
        if self.max < full.1 {
            tokens.push(format!("{key}<={}", trim_number(self.max)));
        }
        if tokens.is_empty() {
            tokens.push(format!("{key}>={}", trim_number(self.min)));
        }
        tokens
    }
}

fn trim_number(value: f32) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Game mode picked from a dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ModeFilter {
    /// osu!standard (also the default, matching previous behaviour).
    #[default]
    Osu,
    Any,
    Taiko,
    Catch,
    Mania,
}

impl ModeFilter {
    pub const ALL: [Self; 5] = [
        Self::Osu,
        Self::Any,
        Self::Taiko,
        Self::Catch,
        Self::Mania,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Osu => "osu! (standard)",
            Self::Any => "Any mode",
            Self::Taiko => "taiko",
            Self::Catch => "catch",
            Self::Mania => "mania",
        }
    }

    pub fn token(self) -> Option<&'static str> {
        match self {
            Self::Osu => Some("osu"),
            Self::Any => None,
            Self::Taiko => Some("taiko"),
            Self::Catch => Some("catch"),
            Self::Mania => Some("mania"),
        }
    }

    pub fn matches(self, mode: Option<u8>) -> bool {
        match self {
            Self::Any => true,
            // A missing Mode field means osu!std in the .osu format.
            Self::Osu => mode.unwrap_or(0) == 0,
            Self::Taiko => mode == Some(1),
            Self::Catch => mode == Some(2),
            Self::Mania => mode == Some(3),
        }
    }
}

/// All collection filters. Text is matched case-insensitively when non-empty;
/// the song length bounds are typed in seconds and ignored when blank or
/// invalid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BeatmapFilters {
    pub artist: String,
    pub title: String,
    pub mapper: String,
    pub difficulty: String,
    pub tag: String,
    pub length_min: String,
    pub length_max: String,
    pub stars: RangeFilter,
    pub ar: RangeFilter,
    pub cs: RangeFilter,
    pub od: RangeFilter,
    pub hp: RangeFilter,
    pub bpm: RangeFilter,
    pub mode: ModeFilter,
}

impl BeatmapFilters {
    pub fn with_full_ranges() -> Self {
        Self {
            stars: RangeFilter::new(STARS_RANGE.0, STARS_RANGE.1),
            ar: RangeFilter::new(AR_RANGE.0, AR_RANGE.1),
            cs: RangeFilter::new(CS_RANGE.0, CS_RANGE.1),
            od: RangeFilter::new(OD_RANGE.0, OD_RANGE.1),
            hp: RangeFilter::new(HP_RANGE.0, HP_RANGE.1),
            bpm: RangeFilter::new(BPM_RANGE.0, BPM_RANGE.1),
            ..Self::default()
        }
    }

    pub fn matches_local(&self, map: &LocalBeatmap) -> bool {
        contains(&map.artist, &self.artist)
            && contains(&map.title, &self.title)
            && contains(&map.creator, &self.mapper)
            && contains(&map.version, &self.difficulty)
            && contains(&map.tags, &self.tag)
            && within_length(map.length_seconds, &self.length_min, &self.length_max)
            && self.stars.matches(map.stars)
            && self.ar.matches(map.ar)
            && self.cs.matches(map.cs)
            && self.od.matches(map.od)
            && self.hp.matches(map.hp)
            && self.bpm.matches(map.bpm)
            && self.mode.matches(map.mode)
    }

    /// Number of active filters, for the sidebar badge.
    pub fn active_count(&self) -> usize {
        let mut count = 0;
        for text in [&self.artist, &self.title, &self.mapper, &self.difficulty, &self.tag] {
            if !text.trim().is_empty() {
                count += 1;
            }
        }
        for range in [&self.stars, &self.ar, &self.cs, &self.od, &self.hp, &self.bpm] {
            if range.enabled {
                count += 1;
            }
        }
        if parse_bound(&self.length_min).is_some() || parse_bound(&self.length_max).is_some() {
            count += 1;
        }
        if self.mode != ModeFilter::Any {
            count += 1;
        }
        count
    }

    pub fn clear_all(&mut self) {
        let fresh = Self::with_full_ranges();
        self.artist.clear();
        self.title.clear();
        self.mapper.clear();
        self.difficulty.clear();
        self.tag.clear();
        self.length_min.clear();
        self.length_max.clear();
        self.stars = fresh.stars;
        self.ar = fresh.ar;
        self.cs = fresh.cs;
        self.od = fresh.od;
        self.hp = fresh.hp;
        self.bpm = fresh.bpm;
        self.mode = ModeFilter::Any;
    }

    /// True when the length boxes are empty or parse as numbers.
    pub fn length_valid(&self) -> bool {
        let min_ok = self.length_min.trim().is_empty() || parse_bound(&self.length_min).is_some();
        let max_ok = self.length_max.trim().is_empty() || parse_bound(&self.length_max).is_some();
        min_ok && max_ok
    }

    /// osu!web-style text for the active filters, shown read-only in the UI.
    pub fn to_osu_search(&self) -> String {
        let mut tokens = Vec::new();
        push_text_token(&mut tokens, "artist", &self.artist);
        push_text_token(&mut tokens, "title", &self.title);
        push_text_token(&mut tokens, "creator", &self.mapper);
        push_text_token(&mut tokens, "difficulty", &self.difficulty);
        push_text_token(&mut tokens, "tag", &self.tag);
        if let Some(min) = parse_bound(&self.length_min) {
            tokens.push(format!("length>={}", trim_number(min)));
        }
        if let Some(max) = parse_bound(&self.length_max) {
            tokens.push(format!("length<={}", trim_number(max)));
        }
        tokens.extend(self.stars.tokens("stars", STARS_RANGE));
        tokens.extend(self.ar.tokens("ar", AR_RANGE));
        tokens.extend(self.cs.tokens("cs", CS_RANGE));
        tokens.extend(self.od.tokens("od", OD_RANGE));
        tokens.extend(self.hp.tokens("hp", HP_RANGE));
        tokens.extend(self.bpm.tokens("bpm", BPM_RANGE));
        if let Some(mode) = self.mode.token() {
            tokens.push(format!("mode={mode}"));
        }
        tokens.join(" ")
    }
}

fn contains(haystack: &str, needle: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return true;
    }
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn parse_bound(text: &str) -> Option<f32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<f32>().ok().filter(|value| value.is_finite())
}

fn within_length(length_seconds: Option<f32>, min_text: &str, max_text: &str) -> bool {
    let min = parse_bound(min_text);
    let max = parse_bound(max_text);
    if min.is_none() && max.is_none() {
        return true;
    }
    let Some(length) = length_seconds else {
        return false;
    };
    min.is_none_or(|min| length >= min) && max.is_none_or(|max| length <= max)
}

fn push_text_token(tokens: &mut Vec<String>, key: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    tokens.push(format!("{key}={}", quote_if_needed(value)));
}

fn quote_if_needed(value: &str) -> String {
    if value.contains(' ') || value.contains('/') || value.contains('"') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters() -> BeatmapFilters {
        BeatmapFilters::with_full_ranges()
    }

    #[test]
    fn builds_readable_osu_query() {
        let mut query = filters();
        query.artist = "Camellia".into();
        query.stars.enabled = true;
        query.stars.min = 5.5;

        assert_eq!(
            query.to_osu_search(),
            "artist=Camellia stars>=5.5 mode=osu"
        );
    }

    #[test]
    fn empty_filters_match_everything_except_mode() {
        let query = filters();
        let map = LocalBeatmap {
            artist: "Camellia".into(),
            mode: None,
            ..Default::default()
        };
        assert!(query.matches_local(&map));

        let taiko = LocalBeatmap {
            mode: Some(1),
            ..Default::default()
        };
        assert!(!query.matches_local(&taiko));
    }

    #[test]
    fn ranges_and_length_behave() {
        let mut query = filters();
        query.mode = ModeFilter::Any;
        query.stars.enabled = true;
        query.stars.min = 5.0;
        query.stars.max = 7.0;
        query.length_min = "60".into();
        query.length_max = "not a number".into();

        let inside = LocalBeatmap {
            stars: Some(6.0),
            length_seconds: Some(120.0),
            ..Default::default()
        };
        assert!(query.matches_local(&inside));

        let too_easy = LocalBeatmap {
            stars: Some(4.9),
            length_seconds: Some(120.0),
            ..Default::default()
        };
        assert!(!query.matches_local(&too_easy));

        // Missing values never match an enabled numeric filter.
        let unknown = LocalBeatmap::default();
        assert!(!query.matches_local(&unknown));
    }
}
