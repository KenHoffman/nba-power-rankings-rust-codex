use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate, Utc};
use reqwest::{
    blocking::{Client, Response},
    header::{ACCEPT, ACCEPT_LANGUAGE},
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

const CATEGORY_URL: &str = "https://www.nba.com/news/category/power-rankings";
const SCHEDULE_URL: &str = "https://cdn.nba.com/static/json/staticData/scheduleLeagueV2.json";

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let client = Client::builder()
        .user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
        )
        .build()
        .context("failed to build HTTP client")?;

    let latest_slug = fetch_latest_power_rankings_slug(&client)?;
    let rankings = fetch_power_rankings(&client, &latest_slug)?;
    let schedule = fetch_schedule(&client)?;

    let today = Utc::now().date_naive();
    let cutoff = today + Duration::days(7);

    let mut ranked: Vec<_> = rankings
        .into_iter()
        .filter_map(|entry| {
            let rank = entry.current_week_rank?;
            let team_id = entry.team_id?;
            let team_name = entry
                .team_name
                .or(entry.team_nickname.clone())
                .or(entry.team_display_name.clone())?;
            Some(ResolvedRanking {
                rank,
                team_id,
                team_name,
            })
        })
        .collect();

    if ranked.is_empty() {
        bail!("no ranked teams found in the latest power rankings article");
    }

    ranked.sort_by_key(|r| r.rank);

    let top_four = ranked.into_iter().take(4).collect::<Vec<_>>();
    let upcoming_games_index = build_upcoming_games_index(&schedule, today, cutoff);

    for team in top_four {
        println!("{} (Rank {})", team.team_name, team.rank);
        if let Some(games) = upcoming_games_index.get(&team.team_id) {
            if games.is_empty() {
                println!("  No games scheduled in the next 7 days.");
            } else {
                for game in games {
                    let location = if game.is_home { "vs" } else { "@" };
                    println!(
                        "  {} {} {}",
                        game.date.format("%Y-%m-%d"),
                        location,
                        game.opponent
                    );
                }
            }
        } else {
            println!("  No games scheduled in the next 7 days.");
        }
        println!();
    }

    Ok(())
}

fn fetch_latest_power_rankings_slug(client: &Client) -> Result<String> {
    let body = fetch_text(
        client
            .get(CATEGORY_URL)
            .header(ACCEPT, "text/html,application/xhtml+xml,application/json")
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9"),
    )
    .context("failed to fetch power rankings category page")?;

    let data: CategoryResponse = extract_next_data(&body)?;
    let slug = data
        .props
        .page_props
        .category
        .latest
        .items
        .into_iter()
        .find_map(|item| {
            let slug = item.slug.trim();
            (!slug.is_empty()).then(|| slug.to_string())
        })
        .context("no articles found in power rankings category")?;

    Ok(slug)
}

fn fetch_power_rankings(client: &Client, slug: &str) -> Result<Vec<PowerRankingEntry>> {
    let url = format!("https://www.nba.com/news/{slug}");
    let body = fetch_text(
        client
            .get(&url)
            .header(ACCEPT, "text/html,application/xhtml+xml,application/json")
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9"),
    )
    .with_context(|| format!("failed to fetch power rankings article at {url}"))?;

    let data: ArticleResponse = extract_next_data(&body)?;
    let entries = data.props.page_props.article.power_rankings;
    Ok(entries)
}

fn fetch_schedule(client: &Client) -> Result<ScheduleResponse> {
    let body = fetch_text(
        client
            .get(SCHEDULE_URL)
            .header(ACCEPT, "application/json")
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9"),
    )
    .context("failed to fetch league schedule")?;

    let schedule: ScheduleResponse =
        serde_json::from_str(&body).context("failed to parse league schedule JSON")?;
    Ok(schedule)
}

fn fetch_text(builder: reqwest::blocking::RequestBuilder) -> Result<String> {
    let response: Response = builder
        .send()
        .context("HTTP request failed")?
        .error_for_status()
        .context("unsuccessful HTTP status code")?;
    response.text().context("failed to read response body")
}

fn extract_next_data<T>(body: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let marker = "<script id=\"__NEXT_DATA__\" type=\"application/json\">";
    let start = body
        .find(marker)
        .context("unable to locate __NEXT_DATA__ script tag")?
        + marker.len();
    let remainder = &body[start..];
    let end = remainder
        .find("</script>")
        .context("unable to locate end of __NEXT_DATA__ script tag")?;
    let json_str = &remainder[..end];
    let data =
        serde_json::from_str(json_str).context("failed to deserialize __NEXT_DATA__ JSON")?;
    Ok(data)
}

fn build_upcoming_games_index(
    schedule: &ScheduleResponse,
    start: NaiveDate,
    end: NaiveDate,
) -> HashMap<u32, Vec<GameListing>> {
    let mut index: HashMap<u32, Vec<GameListing>> = HashMap::new();

    for game_date in &schedule.league_schedule.game_dates {
        for game in &game_date.games {
            let Some(ref date_str) = game.game_date_utc else {
                continue;
            };
            let Ok(date_time) = chrono::DateTime::parse_from_rfc3339(date_str) else {
                continue;
            };
            let game_day = date_time.date_naive();

            if game_day < start || game_day >= end {
                continue;
            }

            let opponent_for_home = format_team(&game.away_team);
            let opponent_for_away = format_team(&game.home_team);

            if let Some(team_id) = game.home_team.team_id {
                index.entry(team_id).or_default().push(GameListing::new(
                    game_day,
                    opponent_for_home.clone(),
                    true,
                ));
            }

            if let Some(team_id) = game.away_team.team_id {
                index.entry(team_id).or_default().push(GameListing::new(
                    game_day,
                    opponent_for_away.clone(),
                    false,
                ));
            }
        }
    }

    for games in index.values_mut() {
        games.sort_by_key(|game| game.date);
    }

    index
}

fn format_team(team: &ScheduleTeam) -> String {
    match (&team.team_city, &team.team_name) {
        (Some(city), Some(name)) if !city.is_empty() => format!("{city} {name}"),
        (_, Some(name)) if !name.is_empty() => name.clone(),
        _ => "TBD Opponent".to_string(),
    }
}

#[derive(Debug)]
struct ResolvedRanking {
    rank: u32,
    team_id: u32,
    team_name: String,
}

#[derive(Debug)]
struct GameListing {
    date: NaiveDate,
    opponent: String,
    is_home: bool,
}

impl GameListing {
    fn new(date: NaiveDate, opponent: String, is_home: bool) -> Self {
        Self {
            date,
            opponent,
            is_home,
        }
    }
}

#[derive(Deserialize)]
struct CategoryResponse {
    props: CategoryProps,
}

#[derive(Deserialize)]
struct CategoryProps {
    #[serde(rename = "pageProps")]
    page_props: CategoryPageProps,
}

#[derive(Deserialize)]
struct CategoryPageProps {
    category: CategoryData,
}

#[derive(Deserialize)]
struct CategoryData {
    latest: LatestArticles,
}

#[derive(Deserialize)]
struct LatestArticles {
    items: Vec<ArticleItem>,
}

#[derive(Deserialize)]
struct ArticleItem {
    slug: String,
}

#[derive(Deserialize)]
struct ArticleResponse {
    props: ArticleProps,
}

#[derive(Deserialize)]
struct ArticleProps {
    #[serde(rename = "pageProps")]
    page_props: ArticlePageProps,
}

#[derive(Deserialize)]
struct ArticlePageProps {
    #[serde(default)]
    article: ArticleData,
}

#[derive(Deserialize, Default)]
struct ArticleData {
    #[serde(rename = "powerRankings", default)]
    power_rankings: Vec<PowerRankingEntry>,
}

#[derive(Deserialize, Debug)]
struct PowerRankingEntry {
    #[serde(rename = "teamId")]
    team_id: Option<u32>,
    #[serde(rename = "teamName")]
    team_name: Option<String>,
    #[serde(rename = "teamNickname")]
    team_nickname: Option<String>,
    #[serde(rename = "teamDisplayName")]
    team_display_name: Option<String>,
    #[serde(rename = "currentWeekRank")]
    current_week_rank: Option<u32>,
}

#[derive(Deserialize)]
struct ScheduleResponse {
    #[serde(rename = "leagueSchedule")]
    league_schedule: LeagueSchedule,
}

#[derive(Deserialize)]
struct LeagueSchedule {
    #[serde(rename = "gameDates")]
    game_dates: Vec<GameDate>,
}

#[derive(Deserialize)]
struct GameDate {
    #[serde(rename = "games")]
    games: Vec<Game>,
}

#[derive(Deserialize)]
struct Game {
    #[serde(rename = "gameDateUTC")]
    game_date_utc: Option<String>,
    #[serde(rename = "homeTeam")]
    home_team: ScheduleTeam,
    #[serde(rename = "awayTeam")]
    away_team: ScheduleTeam,
}

#[derive(Deserialize)]
struct ScheduleTeam {
    #[serde(rename = "teamId")]
    team_id: Option<u32>,
    #[serde(rename = "teamCity")]
    team_city: Option<String>,
    #[serde(rename = "teamName")]
    team_name: Option<String>,
}
