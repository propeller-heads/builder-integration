//! Client for the 1inch Fusion public orders API.

use anyhow::{bail, Context};
use chrono::DateTime;
use reqwest::Client;
use serde::Deserialize;

use crate::order::{AuctionPoint, FusionOrder};

/// Integer divisor for `initialRateBump` and auction point `coefficient` fields.
/// 1inch encodes rate bumps as integer tenths-of-a-millionth: `50_000` = 0.5%.
const RATE_BUMP_DIVISOR: u128 = 10_000_000;

fn known_decimals(address_lower: &str) -> Option<u8> {
    match address_lower {
        // WETH, DAI — 18 decimals
        "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2" | "0x6b175474e89094c44da98b954eedeac495271d0f" => Some(18),
        // USDC, USDT — 6 decimals
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" | "0xdac17f958d2ee523a2206206994597c13d831ec7" => Some(6),
        "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599" => Some(8), // WBTC
        _ => None,
    }
}

/// Computes `base * (1 + bump / RATE_BUMP_DIVISOR)` using ceiling integer arithmetic.
///
/// Matches the on-chain `calcAuctionTakingAmount` which uses `mulDiv(..., Ceil)`.
/// Ceiling: add `RATE_BUMP_DIVISOR - 1` before truncating division.
fn apply_rate_bump(base: u128, bump: u128) -> u128 {
    let increment = base
        .saturating_mul(bump)
        .saturating_add(RATE_BUMP_DIVISOR - 1)
        / RATE_BUMP_DIVISOR;
    base.saturating_add(increment)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveOrdersResponse {
    items: Vec<ActiveOrderItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveOrderItem {
    order_hash: String,
    order: OrderFields,
    auction_start_date: String,
    auction_end_date: String,
    #[serde(default)]
    initial_rate_bump: u64,
    #[serde(default)]
    points: Vec<ApiAuctionPoint>,
    #[serde(default)]
    from_token_to_usdc_rate: Option<f64>,
    #[serde(default)]
    to_token_to_usdc_rate: Option<f64>,
    #[serde(default)]
    from_token_symbol: Option<String>,
    #[serde(default)]
    to_token_symbol: Option<String>,
    #[serde(default)]
    from_token_decimals: Option<u8>,
    #[serde(default)]
    to_token_decimals: Option<u8>,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    extension: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderFields {
    maker_asset: String,
    taker_asset: String,
    making_amount: String,
    taking_amount: String,
    #[serde(default)]
    salt: String,
    #[serde(default)]
    maker: String,
    #[serde(default)]
    receiver: String,
    #[serde(default)]
    maker_traits: String,
}

#[derive(Debug, Deserialize)]
struct ApiAuctionPoint {
    delay: u64,
    coefficient: u128,
}

/// Thin wrapper around `reqwest` for the 1inch Fusion active-orders endpoint.
pub struct OneinchClient {
    http: Client,
    chain_id: u64,
}

impl OneinchClient {
    pub fn new(chain_id: u64) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, chain_id })
    }

    /// Fetches all currently active Fusion orders for the configured chain.
    pub async fn fetch_active_orders(&self) -> anyhow::Result<Vec<FusionOrder>> {
        let url = format!(
            "https://fusion.1inch.io/orders/v2.0/{}/order/active?limit=500",
            self.chain_id
        );

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("GET active orders")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("1inch API returned {status}: {body}");
        }

        let data: ActiveOrdersResponse = resp.json().await.context("parsing active orders")?;

        let mut orders = Vec::with_capacity(data.items.len());
        for item in data.items {
            match convert(item) {
                Ok(order) => orders.push(order),
                Err(e) => tracing::warn!("skipping malformed order: {e}"),
            }
        }
        Ok(orders)
    }

}

fn convert(item: ActiveOrderItem) -> anyhow::Result<FusionOrder> {
    let making_amount: u128 = item.order.making_amount.parse().context("makingAmount")?;
    let taking_amount: u128 = item.order.taking_amount.parse().context("takingAmount")?;
    if taking_amount == 0 {
        bail!("takingAmount is zero");
    }

    let auction_start_ts = parse_iso_timestamp(&item.auction_start_date)
        .with_context(|| format!("auctionStartDate {:?}", item.auction_start_date))?;
    let auction_end_ts = parse_iso_timestamp(&item.auction_end_date)
        .with_context(|| format!("auctionEndDate {:?}", item.auction_end_date))?;
    let api_duration_secs = auction_end_ts
        .checked_sub(auction_start_ts)
        .context("auctionEndDate before auctionStartDate")?;
    if api_duration_secs == 0 {
        bail!("zero-length auction");
    }

    // The 1inch API's auction parameters (auctionStartDate, auctionEndDate, initialRateBump)
    // don't always match what's encoded in the Fusion extension bytes, causing systematic
    // errors in amount_at_timestamp.  Decode the canonical values directly from the extension.
    //
    // Extension layout (hex char offsets, no "0x" prefix):
    //   [0:64]   32-byte LOP header (section lengths)
    //   [64:104] 20-byte Dutch-auction extension address
    //   [104:110] gasBumpEstimate  (uint24)
    //   [110:118] gasPriceEstimate (uint32)  ← 4 bytes
    //   [118:126] startTime        (uint32)
    //   [126:132] duration         (uint24)  ← 180 s for typical orders
    //   [132:138] initialRateBump  (uint24)
    let ext_params = decode_extension_params(&item.extension);
    let auction_start_ts = ext_params.as_ref().map(|p| p.start_time).unwrap_or(auction_start_ts);
    let auction_duration_secs = ext_params.as_ref().map(|p| p.duration).unwrap_or(api_duration_secs);
    let eff_initial_rate_bump = ext_params.as_ref()
        .map(|p| u128::from(p.init_rate_bump))
        .unwrap_or_else(|| u128::from(item.initial_rate_bump));
    let gas_bump_estimate       = ext_params.as_ref().map(|p| p.gas_bump_estimate).unwrap_or(0);
    let gas_price_estimate_mwei = ext_params.as_ref().map(|p| p.gas_price_estimate_mwei).unwrap_or(0);

    let auction_start_amount = apply_rate_bump(taking_amount, eff_initial_rate_bump);

    let mut cumulative_delay: u64 = 0;
    let mut points = Vec::with_capacity(item.points.len());
    for p in item.points {
        cumulative_delay += p.delay;
        if cumulative_delay >= auction_duration_secs {
            break;
        }
        let amount = apply_rate_bump(taking_amount, p.coefficient);
        points.push(AuctionPoint { delay_secs: cumulative_delay, amount });
    }

    let from_addr = item.order.maker_asset.to_lowercase();
    let to_addr = item.order.taker_asset.to_lowercase();
    let from_decimals = item
        .from_token_decimals
        .or_else(|| known_decimals(&from_addr))
        .unwrap_or(18);
    let to_decimals = item
        .to_token_decimals
        .or_else(|| known_decimals(&to_addr))
        .unwrap_or(18);

    Ok(FusionOrder {
        order_id: item.order_hash,
        from_token: item.order.maker_asset,
        to_token: item.order.taker_asset,
        making_amount,
        auction_start_amount,
        auction_end_amount: taking_amount,
        auction_duration_secs,
        auction_start_time: auction_start_ts,
        points,
        from_token_symbol: item.from_token_symbol,
        to_token_symbol: item.to_token_symbol,
        from_token_decimals: from_decimals,
        to_token_decimals: to_decimals,
        from_token_usd_rate: item.from_token_to_usdc_rate.unwrap_or(0.0),
        to_token_usd_rate: item.to_token_to_usdc_rate.unwrap_or(0.0),
        gas_bump_estimate,
        gas_price_estimate_mwei,
        signature: item.signature,
        extension: item.extension,
        salt: item.order.salt,
        maker_address: item.order.maker,
        receiver_address: item.order.receiver,
        maker_traits: item.order.maker_traits,
    })
}

/// Decoded parameters from a 1inch Fusion Dutch-auction extension.
struct ExtensionParams {
    start_time: u64,
    duration: u64,
    init_rate_bump: u32,
    gas_bump_estimate: u32,
    gas_price_estimate_mwei: u32,
}

/// Decodes auction parameters from a 1inch Fusion extension hex string.
///
/// Returns `None` if the extension is too short, malformed, or encodes `duration = 0`.
/// Layout (byte offsets, after stripping optional "0x"):
///   [0:32]   LOP section-length header
///   [32:52]  Dutch-auction extension address (20 bytes)
///   [52:55]  gasBumpEstimate  (uint24)  → hex chars [104:110]
///   [55:59]  gasPriceEstimate (uint32)  → hex chars [110:118]
///   [59:63]  startTime        (uint32)  → hex chars [118:126]
///   [63:66]  duration         (uint24)  → hex chars [126:132]
///   [66:69]  initialRateBump  (uint24)  → hex chars [132:138]
fn decode_extension_params(extension_hex: &str) -> Option<ExtensionParams> {
    let raw = extension_hex.strip_prefix("0x").unwrap_or(extension_hex);
    if raw.len() < 138 {
        return None;
    }
    let gas_bump_estimate       = u32::from_str_radix(&raw[104..110], 16).ok()?;
    let gas_price_estimate_mwei = u32::from_str_radix(&raw[110..118], 16).ok()?;
    let start_time              = u64::from_str_radix(&raw[118..126], 16).ok()?;
    let duration                = u64::from_str_radix(&raw[126..132], 16).ok()?;
    let init_rate_bump          = u32::from_str_radix(&raw[132..138], 16).ok()?;
    if duration == 0 {
        None
    } else {
        Some(ExtensionParams { start_time, duration, init_rate_bump, gas_bump_estimate, gas_price_estimate_mwei })
    }
}

fn parse_iso_timestamp(s: &str) -> anyhow::Result<u64> {
    let dt = DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("not a valid RFC 3339 timestamp: {s:?}"))?;
    u64::try_from(dt.timestamp())
        .with_context(|| format!("negative unix timestamp in {s:?}"))
}
