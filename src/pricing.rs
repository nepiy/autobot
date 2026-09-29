use crate::{
    config::parse_usd_amount,
    error::{BotError, Result},
    security::sanitize_external_text,
};
use alloy::primitives::U256;
use reqwest::Client;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    time::Duration,
};

/// USD values are represented as integer micro-dollars throughout the
/// profitability path. This keeps comparisons deterministic and avoids
/// floating-point rounding around the sell threshold.
pub const USD_SCALE: u128 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceSnapshot {
    prices: BTreeMap<String, U256>,
}

impl PriceSnapshot {
    pub fn from_prices(prices: impl IntoIterator<Item = (impl Into<String>, U256)>) -> Self {
        Self {
            prices: prices
                .into_iter()
                .map(|(symbol, price)| (normalize_currency(&symbol.into()), price))
                .collect(),
        }
    }

    fn get(&self, currency: &str) -> Option<U256> {
        self.prices.get(&normalize_currency(currency)).copied()
    }
}

#[derive(Clone)]
pub struct PriceOracle {
    client: Client,
    #[cfg(test)]
    test_prices: Option<PriceSnapshot>,
}

impl std::fmt::Debug for PriceOracle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PriceOracle")
            .finish_non_exhaustive()
    }
}

impl PriceOracle {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| {
                BotError::Transaction(format!("could not create price client: {err}"))
            })?;
        Ok(Self {
            client,
            #[cfg(test)]
            test_prices: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(prices: PriceSnapshot) -> Self {
        Self {
            client: Client::new(),
            test_prices: Some(prices),
        }
    }

    /// Capture a fresh USD price for every currency used by one sell decision.
    ///
    /// Configuration values take precedence over environment values. ETH can
    /// fall back to the configured HTTP oracle; WETH shares ETH's price.
    /// HYPE also has a live HTTP feed. Other currencies, including USDG, must
    /// have an explicit price rather than silently assuming a stablecoin peg.
    pub async fn snapshot(
        &self,
        currencies: &[&str],
        configured_prices: &BTreeMap<String, String>,
    ) -> Result<PriceSnapshot> {
        #[cfg(test)]
        if let Some(prices) = &self.test_prices {
            return Ok(prices.clone());
        }
        let requested = currencies
            .iter()
            .map(|currency| normalize_currency(currency))
            .collect::<BTreeSet<_>>();
        let mut prices = BTreeMap::new();
        let needs_eth = requested.contains("ETH") || requested.contains("WETH");
        let eth_price = if needs_eth {
            let eth = self.load_explicit_price("ETH", configured_prices)?;
            let weth = self.load_explicit_price("WETH", configured_prices)?;
            if eth.is_some() && weth.is_some() && eth != weth {
                return Err(BotError::Config("ETH and WETH prices must agree".into()));
            }
            Some(match eth.or(weth) {
                Some(price) => price,
                None => self.fetch_eth_price().await?,
            })
        } else {
            None
        };
        for currency in requested {
            if matches!(currency.as_str(), "ETH" | "WETH") {
                prices.insert(currency, eth_price.expect("ETH price was resolved"));
                continue;
            }
            let explicit = self.load_explicit_price(&currency, configured_prices)?;
            let eth_alias = if explicit.is_none() && currency == "WETH" {
                self.load_explicit_price("ETH", configured_prices)?
            } else {
                None
            };
            let price = match explicit.or(eth_alias) {
                Some(price) => price,
                None if currency == "ETH" || currency == "WETH" => self.fetch_eth_price().await?,
                None if currency == "HYPE" => self.fetch_hype_price().await?,
                None => {
                    return Err(BotError::Transaction(format!(
                        "no USD price is configured for `{currency}`; set auto_sell.currency_usd_prices.{currency} or {}",
                        env_price_name(&currency)
                    )));
                }
            };
            prices.insert(currency, price);
        }
        // ETH and WETH are economically equivalent for this comparison. Store
        // both aliases when either was requested so later conversion is simple.
        if let Some(price) = prices.get("ETH").or_else(|| prices.get("WETH")).copied() {
            prices.entry("ETH".to_string()).or_insert(price);
            prices.entry("WETH".to_string()).or_insert(price);
        }
        Ok(PriceSnapshot { prices })
    }

    fn load_explicit_price(
        &self,
        currency: &str,
        configured_prices: &BTreeMap<String, String>,
    ) -> Result<Option<U256>> {
        let configured = configured_prices.iter().find_map(|(symbol, value)| {
            (normalize_currency(symbol) == currency).then_some(value.as_str())
        });
        if let Some(value) = configured {
            return parse_price(value).map(Some);
        }
        let name = env_price_name(currency);
        match env::var(&name) {
            Ok(value) if !value.trim().is_empty() => parse_price(&value).map(Some),
            _ => Ok(None),
        }
    }

    pub fn price_for_currency(&self, snapshot: &PriceSnapshot, currency: &str) -> Result<U256> {
        snapshot.get(currency).ok_or_else(|| {
            BotError::Transaction(format!(
                "the USD price snapshot does not contain `{}`",
                normalize_currency(currency)
            ))
        })
    }

    pub fn amount_to_usd(
        &self,
        snapshot: &PriceSnapshot,
        currency: &str,
        amount: U256,
        decimals: u8,
    ) -> Result<U256> {
        let price = self.price_for_currency(snapshot, currency)?;
        let scale = U256::from(10u64)
            .checked_pow(U256::from(decimals))
            .ok_or_else(|| {
                BotError::Transaction("currency decimals exceed U256 precision".into())
            })?;
        amount
            .checked_mul(price)
            .and_then(|value| value.checked_div(scale))
            .ok_or_else(|| {
                BotError::Transaction(format!(
                    "USD conversion overflowed for {amount} {currency} units"
                ))
            })
    }

    /// Round expenses up so a sub-micro-dollar loss cannot pass break-even.
    pub fn cost_to_usd(
        &self,
        snapshot: &PriceSnapshot,
        currency: &str,
        amount: U256,
        decimals: u8,
    ) -> Result<U256> {
        let price = self.price_for_currency(snapshot, currency)?;
        let scale = U256::from(10)
            .checked_pow(U256::from(decimals))
            .ok_or_else(|| {
                BotError::Transaction("currency decimals exceed U256 precision".into())
            })?;
        amount
            .checked_mul(price)
            .map(|value| value.div_ceil(scale))
            .ok_or_else(|| BotError::Transaction("USD cost conversion overflowed".into()))
    }

    async fn fetch_eth_price(&self) -> Result<U256> {
        let endpoint = env::var("PRICE_ORACLE_URL").unwrap_or_else(|_| {
            "https://pro-api.coinmarketcap.com/v3/cryptocurrency/quotes/latest?id=1027&convert=USD"
                .to_string()
        });
        validate_price_endpoint(&endpoint)?;
        let mut request = self
            .client
            .get(&endpoint)
            .header("accept", "application/json")
            .header("user-agent", "nft-mint-bot/0.1");
        if let Some((env_name, header_name)) = oracle_api_key_header(&endpoint)
            && let Ok(api_key) = env::var(env_name)
            && !api_key.trim().is_empty()
        {
            // Provider-specific headers are only attached to their matching
            // hosts; a custom PRICE_ORACLE_URL must never receive a credential
            // accidentally.
            request = request.header(header_name, api_key.trim());
        }
        let response = request
            .send()
            .await
            .map_err(|_| BotError::PriceUnavailable("ETH/USD price request failed".to_string()))?;
        if !response.status().is_success() {
            return Err(BotError::PriceUnavailable(format!(
                "ETH/USD price request returned HTTP {}",
                response.status()
            )));
        }
        let body: Value =
            crate::opensea::read_json_response(response, "ETH/USD price response was invalid")
                .await?;
        let text = if is_coinmarketcap_endpoint(&endpoint) {
            validate_coinmarketcap_status(&body)?;
            parse_coinmarketcap_eth_price(&body)?
        } else {
            parse_coingecko_eth_price(&body)?
        };
        parse_price(&text)
    }

    async fn fetch_hype_price(&self) -> Result<U256> {
        let endpoint = env::var("HYPE_PRICE_ORACLE_URL").unwrap_or_else(|_| {
            "https://pro-api.coinmarketcap.com/v3/cryptocurrency/quotes/latest?id=32196&convert=USD".into()
        });
        validate_price_endpoint(&endpoint)?;
        let mut request = self
            .client
            .get(&endpoint)
            .header("accept", "application/json");
        if let Some((env_name, header_name)) = oracle_api_key_header(&endpoint)
            && let Ok(key) = env::var(env_name)
            && !key.trim().is_empty()
        {
            request = request.header(header_name, key.trim());
        }
        let response = request
            .send()
            .await
            .map_err(|_| BotError::PriceUnavailable("HYPE/USD price request failed".into()))?;
        if !response.status().is_success() {
            return Err(BotError::PriceUnavailable(format!(
                "HYPE/USD price request returned HTTP {}",
                response.status()
            )));
        }
        let body: Value =
            crate::opensea::read_json_response(response, "invalid HYPE/USD price response").await?;
        let text = if is_coinmarketcap_endpoint(&endpoint) {
            validate_coinmarketcap_status(&body)?;
            parse_coinmarketcap_asset_price(&body, "32196", "HYPE")?
        } else {
            numeric_price_text(body.pointer("/hyperliquid/usd").ok_or_else(|| {
                BotError::Transaction("HYPE/USD response missing hyperliquid.usd".into())
            })?)?
        };
        parse_price(&text)
    }
}

fn validate_price_endpoint(endpoint: &str) -> Result<()> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|_| BotError::Config("PRICE_ORACLE_URL is invalid".into()))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !url.username().is_empty()
        || url.password().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(BotError::Config(
            "PRICE_ORACLE_URL requires HTTPS without userinfo (HTTP is allowed on loopback)".into(),
        ));
    }
    Ok(())
}

fn oracle_api_key_header(endpoint: &str) -> Option<(&'static str, &'static str)> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    match url.host_str()? {
        "api.coingecko.com" => Some(("COINGECKO_API_KEY", "x-cg-demo-api-key")),
        "pro-api.coingecko.com" => Some(("COINGECKO_API_KEY", "x-cg-pro-api-key")),
        "api.coinmarketcap.com" | "pro-api.coinmarketcap.com" => {
            Some(("COINMARKETCAP_API_KEY", "X-CMC_PRO_API_KEY"))
        }
        _ => None,
    }
}

fn is_coinmarketcap_endpoint(endpoint: &str) -> bool {
    oracle_api_key_header(endpoint)
        .is_some_and(|(_, header_name)| header_name == "X-CMC_PRO_API_KEY")
}

fn parse_coingecko_eth_price(body: &Value) -> Result<String> {
    let value = body
        .get("ethereum")
        .and_then(|ethereum| ethereum.get("usd"))
        .ok_or_else(|| {
            BotError::Transaction("ETH/USD price response did not contain ethereum.usd".to_string())
        })?;
    numeric_price_text(value)
}

fn parse_coinmarketcap_eth_price(body: &Value) -> Result<String> {
    parse_coinmarketcap_asset_price(body, "1027", "ETH")
}

fn parse_coinmarketcap_asset_price(body: &Value, id: &str, symbol: &str) -> Result<String> {
    let data = body.get("data").ok_or_else(|| {
        BotError::Transaction(format!(
            "{symbol}/USD price response did not contain CoinMarketCap data"
        ))
    })?;
    let asset = match data {
        Value::Object(records) => records.get(id).or_else(|| records.get(symbol)),
        Value::Array(records) => records.iter().find(|record| {
            record.get("id").is_some_and(|value| {
                value.as_str() == Some(id) || value.as_u64().is_some_and(|n| n.to_string() == id)
            }) || record
                .get("symbol")
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(symbol))
        }),
        _ => None,
    }
    .ok_or_else(|| {
        BotError::Transaction(format!(
            "{symbol}/USD price response did not contain CoinMarketCap asset {id}"
        ))
    })?;
    let value = asset
        .get("quote")
        .or_else(|| asset.get("quotes"))
        .and_then(coinmarketcap_usd_quote_price)
        .ok_or_else(|| {
            BotError::Transaction(format!(
                "{symbol}/USD price response did not contain CoinMarketCap quote.USD.price"
            ))
        })?;
    numeric_price_text(value)
}

fn coinmarketcap_usd_quote_price(value: &Value) -> Option<&Value> {
    match value {
        // Some CMC responses use an object keyed by the fiat symbol.
        Value::Object(_) => object_value_case_insensitive(value, "USD").and_then(|usd| {
            usd.get("price")
                .or_else(|| usd.get("last_price"))
                .or_else(|| usd.get("lastPrice"))
        }),
        // The current CMC quotes endpoint returns an array of quote records,
        // each containing `symbol` and `price`.
        Value::Array(records) => records
            .iter()
            .find(|record| {
                record
                    .get("symbol")
                    .and_then(Value::as_str)
                    .is_some_and(|symbol| symbol.eq_ignore_ascii_case("USD"))
                    || record
                        .get("id")
                        .and_then(parse_json_u64)
                        .is_some_and(|id| id == 2781)
            })
            .and_then(|usd| {
                usd.get("price")
                    .or_else(|| usd.get("last_price"))
                    .or_else(|| usd.get("lastPrice"))
            }),
        _ => None,
    }
}

fn object_value_case_insensitive<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object().and_then(|object| {
        object
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    })
}

fn validate_coinmarketcap_status(body: &Value) -> Result<()> {
    let Some(status) = body.get("status") else {
        return Ok(());
    };
    let error_code = status
        .get("error_code")
        .and_then(parse_json_u64)
        .unwrap_or_default();
    if error_code == 0 {
        return Ok(());
    }
    let error_message = status
        .get("error_message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or("request rejected");
    Err(BotError::Transaction(format!(
        "CoinMarketCap API error {error_code}: {}",
        sanitize_external_text(error_message, 256)
    )))
}

fn parse_json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn numeric_price_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        _ => Err(BotError::Transaction(
            "ETH/USD price response contained a non-numeric value".to_string(),
        )),
    }
}

fn normalize_currency(currency: &str) -> String {
    currency.trim().to_ascii_uppercase()
}

fn env_price_name(currency: &str) -> String {
    let symbol = currency
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{symbol}_USD_PRICE")
}

pub fn parse_price(value: &str) -> Result<U256> {
    let value = value.trim();
    // APIs commonly return more than six fractional USD digits. Profitability
    // is stored in micro-dollars, so truncate extra precision (rather than
    // round up) to avoid overstating an offer or understating a cost.
    let normalized = match value.split_once('.') {
        Some((whole, fraction))
            if fraction.len() > 6
                && whole.chars().all(|character| character.is_ascii_digit())
                && fraction.chars().all(|character| character.is_ascii_digit()) =>
        {
            format!("{whole}.{}", &fraction[..6])
        }
        _ => value.to_string(),
    };
    let price = parse_usd_amount(&normalized)
        .map_err(|err| BotError::Transaction(format!("invalid USD price `{value}`: {err}")))?;
    if price.is_zero() {
        return Err(BotError::Transaction(format!(
            "invalid USD price `{value}`: price must be greater than zero"
        )));
    }
    Ok(price)
}

pub fn format_usd(value: U256) -> String {
    let scale = U256::from(USD_SCALE);
    let whole = value / scale;
    let fraction = value % scale;
    format!("${whole}.{:06}", fraction.to::<u64>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_hype_usd_from_mixed_coinmarketcap_assets() {
        for id in [serde_json::json!(32196), serde_json::json!("32196")] {
            let body = serde_json::json!({"data": [
                {"id":1027,"symbol":"ETH","quote":[{"symbol":"USD","price":2000}]},
                {"id":id,"symbol":"HYPE","quote":[{"symbol":"USD","price":40.125}]}
            ]});
            assert_eq!(
                parse_coinmarketcap_asset_price(&body, "32196", "HYPE").unwrap(),
                "40.125"
            );
            assert_eq!(parse_coinmarketcap_eth_price(&body).unwrap(), "2000");
        }
        let body = serde_json::json!({"data":{"32196":{"quote":{"USD":{"price":40}}}}});
        assert_eq!(
            parse_coinmarketcap_asset_price(&body, "32196", "HYPE").unwrap(),
            "40"
        );
        assert!(parse_coinmarketcap_eth_price(&body).is_err());
    }

    #[test]
    fn converts_eth_and_weth_using_the_same_price() {
        let oracle = PriceOracle::new().unwrap();
        let snapshot = PriceSnapshot::from_prices([
            ("ETH", U256::from(3_000_000_000u64)),
            ("WETH", U256::from(3_000_000_000u64)),
        ]);
        let one_eth = U256::from(1_000_000_000_000_000_000u128);
        assert_eq!(
            oracle.amount_to_usd(&snapshot, "ETH", one_eth, 18).unwrap(),
            U256::from(3_000_000_000u64)
        );
        assert_eq!(
            oracle
                .amount_to_usd(&snapshot, "WETH", one_eth, 18)
                .unwrap(),
            U256::from(3_000_000_000u64)
        );
    }

    #[test]
    fn supports_explicit_non_eth_native_prices() {
        let oracle = PriceOracle::new().unwrap();
        let snapshot = PriceSnapshot::from_prices([("POL", U256::from(550_000u64))]);
        let one_pol = U256::from(1_000_000_000_000_000_000u128);
        assert_eq!(
            oracle.amount_to_usd(&snapshot, "POL", one_pol, 18).unwrap(),
            U256::from(550_000u64)
        );
    }

    #[test]
    fn formats_and_parses_micro_dollars() {
        assert_eq!(
            parse_price("123.456789").unwrap(),
            U256::from(123_456_789u64)
        );
        assert_eq!(format_usd(U256::from(123_456_789u64)), "$123.456789");
    }

    #[test]
    fn truncates_high_precision_api_prices_to_micro_dollars() {
        assert_eq!(
            parse_price("2492.250810117154").unwrap(),
            U256::from(2_492_250_810u64)
        );
        assert!(parse_price("0.0000001").is_err());
    }

    #[test]
    fn selects_the_correct_coingecko_api_key_header() {
        assert_eq!(
            oracle_api_key_header("https://api.coingecko.com/api/v3/simple/price"),
            Some(("COINGECKO_API_KEY", "x-cg-demo-api-key"))
        );
        assert_eq!(
            oracle_api_key_header("https://pro-api.coingecko.com/api/v3/simple/price"),
            Some(("COINGECKO_API_KEY", "x-cg-pro-api-key"))
        );
        assert_eq!(
            oracle_api_key_header("https://prices.example.test/eth"),
            None
        );
    }

    #[test]
    fn parses_coinmarketcap_eth_quote() {
        let body = serde_json::json!({
            "data": [
                {
                    "id": 1027,
                    "quote": [
                        { "id": 2781, "symbol": "USD", "price": 2500.125 }
                    ]
                }
            ],
            "status": { "error_code": "0", "error_message": "" }
        });

        assert_eq!(parse_coinmarketcap_eth_price(&body).unwrap(), "2500.125");
        validate_coinmarketcap_status(&body).unwrap();
        assert!(is_coinmarketcap_endpoint(
            "https://pro-api.coinmarketcap.com/v3/cryptocurrency/quotes/latest"
        ));
        assert_eq!(
            oracle_api_key_header(
                "https://pro-api.coinmarketcap.com/v3/cryptocurrency/quotes/latest"
            ),
            Some(("COINMARKETCAP_API_KEY", "X-CMC_PRO_API_KEY"))
        );
    }

    #[test]
    fn reports_coinmarketcap_api_errors_before_parsing_quotes() {
        let body = serde_json::json!({
            "status": {
                "error_code": "1006",
                "error_message": "Your API Key is invalid."
            },
            "data": []
        });
        let error = validate_coinmarketcap_status(&body).unwrap_err();
        assert!(error.to_string().contains("CoinMarketCap API error 1006"));
    }
    #[test]
    fn price_credentials_require_a_secure_exact_provider_host() {
        for url in [
            "http://pro-api.coinmarketcap.com/quotes",
            "https://pro-api.coinmarketcap.com.evil.test/",
            "https://user:pass@pro-api.coinmarketcap.com/",
        ] {
            assert!(oracle_api_key_header(url).is_none());
        }
        assert!(validate_price_endpoint("http://pro-api.coinmarketcap.com/").is_err());
        assert!(validate_price_endpoint("http://[::1]:8080/").is_ok());
    }

    #[tokio::test]
    async fn eth_and_weth_use_one_explicit_snapshot_price() {
        let oracle = PriceOracle::new().unwrap();
        let mut prices = BTreeMap::from([
            ("ETH".into(), "2000".into()),
            ("WETH".into(), "2000".into()),
        ]);
        let snapshot = oracle.snapshot(&["ETH", "WETH"], &prices).await.unwrap();
        assert_eq!(snapshot.get("ETH"), snapshot.get("WETH"));
        prices.insert("WETH".into(), "2001".into());
        assert!(oracle.snapshot(&["ETH", "WETH"], &prices).await.is_err());
    }

    #[test]
    fn decimals_overflow_returns_an_error_without_panicking() {
        let oracle = PriceOracle::new().unwrap();
        let prices = PriceSnapshot::from_prices([("ETH", U256::from(1_000_000))]);
        assert!(
            oracle
                .amount_to_usd(&prices, "ETH", U256::from(1), 78)
                .is_err()
        );
        assert!(
            oracle
                .cost_to_usd(&prices, "ETH", U256::from(1), 255)
                .is_err()
        );
    }
}
