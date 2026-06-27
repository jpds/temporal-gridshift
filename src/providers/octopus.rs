use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{PriceProvider, PricedWindow, ProviderError};

pub struct OctopusProvider {
    client: Client,
    /// Cached (token, tariff_code) to avoid re-authenticating when today and tomorrow
    /// are fetched in parallel. Invalidated on 401 so the next retry re-authenticates.
    auth_cache: Mutex<Option<(String, String)>>,
}

impl OctopusProvider {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            auth_cache: Mutex::new(None),
        }
    }

    /// Octopus publishes next-day Agile prices at 16:00 local time.
    fn refresh_time() -> NaiveTime {
        NaiveTime::from_hms_opt(16, 0, 0).unwrap()
    }

    /// Resolve (token, tariff_code), using the cache when available.
    async fn resolve_auth(&self) -> Result<(String, String), ProviderError> {
        let mut cache = self.auth_cache.lock().await;
        if let Some(ref cached) = *cache {
            return Ok(cached.clone());
        }

        let api_key = std::env::var("OCTOPUS_API_KEY")
            .map_err(|e| anyhow::anyhow!("OCTOPUS_API_KEY not set: {e}"))?;
        let token = authenticate(&self.client, &api_key)
            .await
            .map_err(ProviderError::Other)?;

        let account_numbers = get_account_numbers(&self.client, &token)
            .await
            .map_err(ProviderError::Other)?;
        if account_numbers.is_empty() {
            return Err(ProviderError::Other(anyhow::anyhow!(
                "No accounts found for the provided API key"
            )));
        }

        let mut tariff_code_found = None;
        for number in &account_numbers {
            let account = get_account(&self.client, &token, number)
                .await
                .map_err(ProviderError::Other)?;
            if let Some(code) = find_agile_tariff(&account) {
                tariff_code_found = Some(code.to_owned());
                break;
            }
        }
        let tariff_code = tariff_code_found.ok_or_else(|| {
            ProviderError::Other(anyhow::anyhow!(
                "No active Agile tariff found across all accounts"
            ))
        })?;

        let parts: Vec<&str> = tariff_code.split('-').collect();
        if parts.len() < 4 {
            return Err(ProviderError::Other(anyhow::anyhow!(
                "unexpected tariff code format {:?} (expected at least 4 dash-separated segments, e.g. E-1R-AGILE-FLEX-22-11-25-C)",
                tariff_code
            )));
        }

        *cache = Some((token.clone(), tariff_code.clone()));
        Ok((token, tariff_code))
    }
}

impl Default for OctopusProvider {
    fn default() -> Self {
        Self::new()
    }
}

// GraphQL structs for token exchange and account discovery.
#[derive(Serialize)]
struct GraphqlRequest<'a, V: Serialize> {
    query: &'a str,
    variables: V,
}

#[derive(Serialize)]
struct AuthVars<'a> {
    input: AuthInput<'a>,
}

#[derive(Serialize)]
struct AuthInput<'a> {
    #[serde(rename = "APIKey")]
    api_key: &'a str,
}

#[derive(Deserialize)]
struct AuthResponse {
    data: AuthData,
}

#[derive(Deserialize)]
struct AuthData {
    #[serde(rename = "obtainKrakenToken")]
    token: TokenWrapper,
}

#[derive(Deserialize)]
struct TokenWrapper {
    token: String,
}

#[derive(Serialize)]
struct NoVars;

#[derive(Deserialize)]
struct ViewerResponse {
    data: ViewerData,
}

#[derive(Deserialize)]
struct ViewerData {
    viewer: ViewerUser,
}

#[derive(Deserialize)]
struct ViewerUser {
    accounts: Vec<AccountSummary>,
}

#[derive(Deserialize)]
struct AccountSummary {
    number: String,
}

// Minimal account structs; we only need the active Agile tariff code.
#[derive(Deserialize)]
struct Account {
    properties: Vec<Property>,
}

#[derive(Deserialize)]
struct Property {
    electricity_meter_points: Vec<ElectricityMeterPoint>,
}

#[derive(Deserialize)]
struct ElectricityMeterPoint {
    agreements: Vec<Agreement>,
}

#[derive(Deserialize)]
struct Agreement {
    tariff_code: String,
    #[serde(default)]
    valid_from: Option<DateTime<Utc>>,
    #[serde(default)]
    valid_to: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct OctopusResponse {
    results: Vec<OctopusRate>,
}

#[derive(Deserialize)]
struct OctopusRate {
    value_inc_vat: f64,
    valid_from: DateTime<Utc>,
}

async fn authenticate(client: &Client, api_key: &str) -> anyhow::Result<String> {
    let body = GraphqlRequest {
        query: "mutation ObtainToken($input: ObtainJSONWebTokenInput!) { \
                  obtainKrakenToken(input: $input) { token } }",
        variables: AuthVars {
            input: AuthInput { api_key },
        },
    };
    let resp: AuthResponse = client
        .post("https://api.octopus.energy/v1/graphql/")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.data.token.token)
}

async fn get_account_numbers(client: &Client, token: &str) -> anyhow::Result<Vec<String>> {
    let body = GraphqlRequest {
        query: "{ viewer { accounts { number } } }",
        variables: NoVars,
    };
    let resp: ViewerResponse = client
        .post("https://api.octopus.energy/v1/graphql/")
        .header("Authorization", token)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp
        .data
        .viewer
        .accounts
        .into_iter()
        .map(|a| a.number)
        .collect())
}

async fn get_account(
    client: &Client,
    token: &str,
    account_number: &str,
) -> anyhow::Result<Account> {
    let url = format!("https://api.octopus.energy/v1/accounts/{account_number}/");
    Ok(client
        .get(&url)
        .header("Authorization", token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

fn find_agile_tariff(account: &Account) -> Option<&str> {
    let now = Utc::now();
    account
        .properties
        .iter()
        .flat_map(|p| &p.electricity_meter_points)
        .flat_map(|mp| &mp.agreements)
        .filter(|a| {
            a.valid_from.is_none_or(|dt| dt <= now)
                && a.valid_to.is_none_or(|dt| dt > now)
                && !a.tariff_code.contains("OUTGOING")
        })
        .map(|a| a.tariff_code.as_str())
        .find(|t| t.contains("AGILE"))
}

#[async_trait]
impl PriceProvider for OctopusProvider {
    fn next_day_data_available_at(&self) -> Option<NaiveTime> {
        Some(Self::refresh_time())
    }

    async fn fetch_priced_windows(
        &self,
        date: NaiveDate,
        timezone: &str,
    ) -> Result<Vec<PricedWindow>, ProviderError> {
        let tz: Tz = timezone
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid timezone {timezone:?}"))?;

        let (_token, tariff_code) = self.resolve_auth().await?;

        // Derive product code: "E-1R-AGILE-FLEX-22-11-25-C" -> "AGILE-FLEX-22-11-25"
        let parts: Vec<&str> = tariff_code.split('-').collect();
        let product_code = parts[2..parts.len() - 1].join("-");

        // Use local midnight as the query boundary so the full local day (48 slots) is returned.
        // Querying by UTC midnight clips 2 slots at each end for UTC+ timezones.
        let midnight = |d: chrono::NaiveDate| {
            tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                .earliest()
                .map(|dt| {
                    dt.with_timezone(&chrono::Utc)
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string()
                })
                .unwrap_or_else(|| format!("{d}T00:00:00Z"))
        };
        let period_from = midnight(date);
        let period_to = midnight(date.succ_opt().expect("date overflow"));

        let url = format!(
            "https://api.octopus.energy/v1/products/{product_code}/electricity-tariffs/{tariff_code}/standard-unit-rates/",
        );

        let response = self
            .client
            .get(&url)
            .query(&[("period_from", &period_from), ("period_to", &period_to)])
            .send()
            .await
            .map_err(|e| ProviderError::Other(anyhow::anyhow!(e)))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Token expired; invalidate the cache so the next activity retry re-authenticates.
            *self.auth_cache.lock().await = None;
            return Err(ProviderError::Other(anyhow::anyhow!(
                "Octopus API returned 401: token expired, will re-authenticate on retry"
            )));
        }

        let resp: OctopusResponse = response
            .error_for_status()
            .map_err(|e| ProviderError::Other(anyhow::anyhow!(e)))?
            .json()
            .await
            .map_err(|e| ProviderError::Other(anyhow::anyhow!(e)))?;

        if resp.results.is_empty() {
            return Err(ProviderError::NotYetPublished { date });
        }

        for rate in &resp.results {
            if !rate.value_inc_vat.is_finite() {
                return Err(ProviderError::Other(anyhow::anyhow!(
                    "Octopus API returned non-finite price ({}) for slot starting {}",
                    rate.value_inc_vat,
                    rate.valid_from,
                )));
            }
        }

        let mut rates = resp.results;
        rates.sort_by_key(|r| r.valid_from);

        Ok(rates
            .iter()
            .map(|r| {
                let t = r.valid_from.with_timezone(&tz);
                PricedWindow {
                    date: t.date_naive(),
                    hour: t.hour(),
                    minute: t.minute(),
                    price_p_per_kwh: r.value_inc_vat,
                }
            })
            .collect())
    }
}
