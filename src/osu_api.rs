#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};
use tokio::{fs, io::AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct OsuApiClient {
    http: reqwest::Client,
    bearer_token: Option<String>,
}

impl OsuApiClient {
    pub fn new(bearer_token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("osu-map-manager/0.1")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, bearer_token })
    }

    pub async fn search_beatmapsets(
        &self,
        query: &str,
        cursor: Option<&str>,
    ) -> Result<BeatmapsetSearchResponse> {
        let mut url = reqwest::Url::parse("https://osu.ppy.sh/api/v2/beatmapsets/search")?;
        if !query.trim().is_empty() {
            url.query_pairs_mut().append_pair("q", query);
        }
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("cursor_string", cursor);
        }

        let request = self.authorize(self.http.get(url));
        request
            .send()
            .await?
            .error_for_status()?
            .json::<BeatmapsetSearchResponse>()
            .await
            .context("decoding beatmapset search response")
    }

    #[allow(dead_code)]
    pub async fn get_beatmapset(&self, beatmapset_id: i64) -> Result<Beatmapset> {
        let url = format!("https://osu.ppy.sh/api/v2/beatmapsets/{beatmapset_id}");
        self.authorize(self.http.get(url))
            .send()
            .await?
            .error_for_status()?
            .json::<Beatmapset>()
            .await
            .with_context(|| format!("fetching beatmapset {beatmapset_id}"))
    }

    #[allow(dead_code)]
    pub async fn download_beatmapset(&self, beatmapset_id: i64, destination: &Path) -> Result<()> {
        let url = format!("https://osu.ppy.sh/api/v2/beatmapsets/{beatmapset_id}/download");
        let mut response = self
            .authorize(self.http.get(url))
            .send()
            .await?
            .error_for_status()?;
        let mut file = fs::File::create(destination)
            .await
            .with_context(|| format!("creating {}", destination.display()))?;

        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
        }
        Ok(())
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.bearer_token {
            Some(token) if !token.trim().is_empty() => request.bearer_auth(token.trim()),
            _ => request,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BeatmapsetSearchResponse {
    #[serde(default)]
    pub beatmapsets: Vec<Beatmapset>,
    #[serde(default)]
    pub cursor_string: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Beatmapset {
    pub id: i64,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub creator: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub ranked: Option<i32>,
    #[serde(default)]
    pub submitted_date: Option<String>,
    #[serde(default)]
    pub ranked_date: Option<String>,
    #[serde(default)]
    pub last_updated: Option<String>,
    #[serde(default)]
    pub beatmaps: Vec<Beatmap>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Beatmap {
    pub id: i64,
    #[serde(default)]
    pub beatmapset_id: Option<i64>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub difficulty_rating: Option<f32>,
    #[serde(default)]
    pub ar: Option<f32>,
    #[serde(default)]
    pub cs: Option<f32>,
    #[serde(default)]
    pub accuracy: Option<f32>,
    #[serde(default)]
    pub drain: Option<f32>,
    #[serde(default)]
    pub bpm: Option<f32>,
    #[serde(default)]
    pub total_length: Option<i32>,
    #[serde(default)]
    pub hit_length: Option<i32>,
}
