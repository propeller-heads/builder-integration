//! Client for the 1inch Fusion public orders API and on-chain LOP queries.

use std::str::FromStr;

use alloy::primitives::{Address as AlloyAddress, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolError as _};
use anyhow::{bail, Context};
use chrono::DateTime;
use reqwest::Client;
use serde::Deserialize;

use crate::abi::RawOrderFields;
use crate::order::{AuctionPoint, FusionOrder};
use crate::parse_address;

/// 1inch LOP v4 on Ethereum mainnet.
pub(crate) const LOP_V4: AlloyAddress =
    alloy::primitives::address!("111111125421cA6dc452d289314280a0f8842A65");

/// `remainingInvalidatorForOrder(address,bytes32)` selector on LOP v4.
/// Returns the remaining making amount for partially-fillable orders.
/// Returns 0 if fully filled/cancelled, `U256::MAX` if never partially filled.
const REMAINING_SELECTOR: [u8; 4] = [0x10, 0xad, 0x2c, 0x8b];

// ABI for querying the Fusion Dutch-auction extension via LOP.simulate().
// All Order address fields are `uint256` to match 1inch's packed `Address` type.
// The extension's getTakingAmount reads transient storage set by the LOP before
// calling it, so it cannot be called directly — we route through LOP.simulate()
// which calls the extension with msg.sender == LOP.
sol! {
    struct FusionExtOrder {
        uint256 salt;
        uint256 maker;
        uint256 receiver;
        uint256 makerAsset;
        uint256 takerAsset;
        uint256 makingAmount;
        uint256 takingAmount;
        uint256 makerTraits;
    }

    interface IAmountGetter {
        function getTakingAmount(
            FusionExtOrder order,
            bytes extension,
            bytes32 orderHash,
            address taker,
            uint256 makingAmount,
            uint256 remainingMakingAmount,
            bytes extraData
        ) external view returns (uint256);
    }

        // Renamed from IOrderMixin to avoid a name collision with the abi.rs interface.
    interface ILopSimulate {
        /// Calls `target.call(data)` and reverts with `SimulationResults(success, result)`.
        function simulate(address target, bytes data) external;
        error SimulationResults(bool success, bytes result);
    }
}

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
fn apply_rate_bump(base: U256, bump: u128) -> U256 {
    let divisor = U256::from(RATE_BUMP_DIVISOR);
    let increment = base
        .saturating_mul(U256::from(bump))
        .saturating_add(divisor - U256::ONE)
        / divisor;
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

/// Thin wrapper around `reqwest` for the 1inch Fusion API and on-chain LOP queries.
pub struct OneinchClient {
    http: Client,
    chain_id: u64,
    /// Shared HTTP client for all Ethereum JSON-RPC calls.
    rpc_client: Client,
    /// Ethereum JSON-RPC URL for on-chain queries (remaining amount, taking amount).
    rpc_url: String,
}

impl OneinchClient {
    pub fn new(chain_id: u64, rpc_url: String) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("building 1inch HTTP client")?;
        let rpc_client = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .context("building Ethereum RPC HTTP client")?;
        Ok(Self { http, chain_id, rpc_client, rpc_url })
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

    /// Calls `LOP.remainingInvalidatorForOrder(maker, orderHash)` via a raw JSON-RPC
    /// `eth_call` and returns the remaining making amount that can still be filled.
    ///
    /// - Returns `full_making` when the order is fresh (LOP returns `U256::MAX`) or on error.
    /// - Returns `0` when the order is fully filled or cancelled.
    /// - Returns the actual remaining making amount otherwise (clamped to `full_making`).
    pub(crate) async fn query_remaining_making_amount(&self, order: &FusionOrder) -> U256 {
        let full = order.making_amount;

        let maker: AlloyAddress = match order.maker_address.parse() {
            Ok(a) => a,
            Err(_) => return full,
        };

        let hash_str = order.order_id.strip_prefix("0x").unwrap_or(&order.order_id);
        let hash_bytes = match hex::decode(hash_str) {
            Ok(b) if b.len() == 32 => b,
            _ => return full,
        };

        // ABI-encode: remainingInvalidatorForOrder(address maker, bytes32 orderHash)
        // = selector(4B) + address_padded(32B) + bytes32(32B)
        let mut calldata = Vec::with_capacity(68);
        calldata.extend_from_slice(&REMAINING_SELECTOR);
        calldata.extend_from_slice(&[0u8; 12]); // address left-padded to 32 bytes
        calldata.extend_from_slice(maker.as_slice());
        calldata.extend_from_slice(&hash_bytes);

        let lop_hex = format!("0x{}", hex::encode(LOP_V4.as_slice()));
        let data_hex = format!("0x{}", hex::encode(&calldata));
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": lop_hex, "data": data_hex}, "latest"],
            "id": 1
        });

        let Ok(resp) = self.rpc_client.post(&self.rpc_url).json(&body).send().await else {
            return full;
        };

        let json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return full,
        };

        let hex_result = match json["result"].as_str() {
            Some(s) => s.strip_prefix("0x").unwrap_or(s),
            None => return full,
        };

        if hex_result.len() < 64 {
            return full;
        }

        let Ok(raw) = hex::decode(&hex_result[hex_result.len() - 64..]) else { return full };

        let val = U256::from_be_slice(&raw);
        if val == U256::MAX {
            full // fresh order: never partially filled
        } else {
            val.min(full)
        }
    }

    /// Static-calls `extension.getTakingAmount(...)` to get the exact on-chain auction price.
    ///
    /// Returns `None` on RPC error, malformed order data, or call revert (e.g. order expired).
    /// Callers fall back to the off-chain estimate when `None` is returned.
    pub(crate) async fn query_onchain_taking_amount(
        &self,
        fusion_order: &FusionOrder,
        fill_making_amount: U256,
        remaining_making_amount: U256,
        resolver: AlloyAddress,
        pending_block_ts: u64,
    ) -> Option<U256> {
        // Extension contract address lives at hex chars [64:104] (bytes [32:52]).
        let ext_hex = fusion_order.extension.strip_prefix("0x").unwrap_or(&fusion_order.extension);
        if ext_hex.len() < 104 {
            return None;
        }
        let ext_addr_bytes = hex::decode(&ext_hex[64..104]).ok()?;
        let ext_addr = AlloyAddress::from_slice(&ext_addr_bytes);

        let hash_str = fusion_order.order_id.strip_prefix("0x").unwrap_or(&fusion_order.order_id);
        let hash_bytes: [u8; 32] = hex::decode(hash_str).ok()?.try_into().ok()?;

        let extension_bytes = hex::decode(ext_hex).ok()?;

        // Extract the TakingAmountData section from the extension header.
        // Header is 32 bytes big-endian uint256; each section's end offset is packed in 32-bit chunks:
        //   bits [95:64]  = MakingAmountData end offset  (header bytes [20:24])
        //   bits [127:96] = TakingAmountData end offset  (header bytes [16:20])
        // Section begin = previous section's end.
        // The first 20 bytes of TakingAmountData are the getter address; the rest is extraData.
        let taking_extra_data: alloy::primitives::Bytes = (|| -> Option<_> {
            let hdr = extension_bytes.get(0..32)?;
            let making_end = u32::from_be_bytes(hdr[20..24].try_into().ok()?) as usize;
            let taking_end = u32::from_be_bytes(hdr[16..20].try_into().ok()?) as usize;
            let begin = 32 + making_end;
            let end   = 32 + taking_end;
            let section = extension_bytes.get(begin..end)?;
            // section[0:20] = getter address, section[20:] = extraData for getTakingAmount
            Some(alloy::primitives::Bytes::copy_from_slice(section.get(20..)?))
        })()
        .unwrap_or_default();

        let inner = IAmountGetter::getTakingAmountCall {
            order: FusionExtOrder {
                salt: U256::from_str(&fusion_order.salt).unwrap_or_default(),
                maker: address_to_u256(&fusion_order.maker_address).ok()?,
                receiver: address_to_u256(&fusion_order.receiver_address).ok()?,
                makerAsset: address_to_u256(&fusion_order.from_token).ok()?,
                takerAsset: address_to_u256(&fusion_order.to_token).ok()?,
                makingAmount: fusion_order.making_amount,
                takingAmount: fusion_order.auction_end_amount,
                makerTraits: U256::from_str(&fusion_order.maker_traits).unwrap_or_default(),
            },
            extension: alloy::primitives::Bytes::from(extension_bytes),
            orderHash: alloy::primitives::FixedBytes::from(hash_bytes),
            taker: resolver,
            makingAmount: fill_making_amount,
            remainingMakingAmount: remaining_making_amount,
            extraData: taking_extra_data,
        };

        // Wrap in LOP.simulate(extension_addr, inner_calldata).
        // The LOP calls the extension with msg.sender == LOP and reverts with
        // SimulationResults(success, abi.encode(taking_amount)).
        let simulate_call = ILopSimulate::simulateCall {
            target: ext_addr,
            data: alloy::primitives::Bytes::from(inner.abi_encode()),
        };
        let lop_hex  = format!("0x{}", hex::encode(LOP_V4.as_slice()));
        let data_hex = format!("0x{}", hex::encode(simulate_call.abi_encode()));

        // Pass the pending block timestamp so the extension sees the same elapsed time our
        // off-chain estimate used.  Without this, eth_call runs at the confirmed block
        // timestamp (≈12 s earlier), which can make the auction appear not yet started and
        // return the full start price — inflating the required taking amount.
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [
                {"to": lop_hex, "data": data_hex},
                "latest",
                {},
                {"time": format!("0x{:x}", pending_block_ts)}
            ],
            "id": 1
        });

        let resp = self.rpc_client.post(&self.rpc_url).json(&body).send().await.ok()?;
        let json: serde_json::Value = resp.json().await.ok()?;

        // simulate() ALWAYS reverts — expected revert is SimulationResults(success, result).
        let err = json.get("error")?;
        let revert_data_str = err.get("data").and_then(|d| d.as_str()).unwrap_or("");
        let revert_hex = revert_data_str.strip_prefix("0x").unwrap_or(revert_data_str);
        let revert_bytes = hex::decode(revert_hex).ok()?;

        let sim = ILopSimulate::SimulationResults::abi_decode(&revert_bytes).ok()?;
        if !sim.success {
            tracing::debug!(
                order_id = %fusion_order.order_id,
                "getTakingAmount inner call reverted via simulate()",
            );
            return None;
        }

        if sim.result.len() < 32 {
            return None;
        }
        Some(U256::from_be_slice(&sim.result[..32]))
    }
}

/// Encodes a [`FusionOrder`]'s fields into [`RawOrderFields`] for ABI encoding.
pub(crate) fn build_order_fields(fusion: &FusionOrder) -> anyhow::Result<RawOrderFields> {
    Ok(RawOrderFields {
        salt: U256::from_str(&fusion.salt)
            .map_err(|e| anyhow::anyhow!("invalid order salt {:?}: {e}", fusion.salt))?,
        maker: address_to_u256(&fusion.maker_address)?,
        receiver: address_to_u256(&fusion.receiver_address)?,
        maker_asset: address_to_u256(&fusion.from_token)?,
        taker_asset: address_to_u256(&fusion.to_token)?,
        making_amount: fusion.making_amount,
        taking_amount: fusion.auction_end_amount,
        maker_traits: U256::from_str(&fusion.maker_traits)
            .map_err(|e| anyhow::anyhow!("invalid maker_traits {:?}: {e}", fusion.maker_traits))?,
    })
}

pub(crate) fn address_to_u256(hex: &str) -> anyhow::Result<U256> {
    let addr = parse_address(hex)?;
    Ok(U256::from_be_slice(addr.as_ref()))
}

pub(crate) fn hex_to_bytes(s: &str) -> anyhow::Result<Vec<u8>> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    if stripped.is_empty() {
        return Ok(vec![]);
    }
    hex::decode(stripped).map_err(|e| anyhow::anyhow!("hex decode: {e}"))
}

fn convert(item: ActiveOrderItem) -> anyhow::Result<FusionOrder> {
    let making_amount = U256::from(
        item.order.making_amount.parse::<u128>().context("makingAmount")?
    );
    let taking_amount = U256::from(
        item.order.taking_amount.parse::<u128>().context("takingAmount")?
    );
    if taking_amount.is_zero() {
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
    let auction_start_ts = ext_params.as_ref().map_or(auction_start_ts, |p| p.start_time);
    let auction_duration_secs = ext_params.as_ref().map_or(api_duration_secs, |p| p.duration);
    let eff_initial_rate_bump: u128 = ext_params.as_ref()
        .map_or_else(|| u128::from(item.initial_rate_bump), |p| u128::from(p.init_rate_bump));
    let gas_bump_estimate       = ext_params.as_ref().map_or(0, |p| p.gas_bump_estimate);
    let gas_price_estimate_mwei = ext_params.as_ref().map_or(0, |p| p.gas_price_estimate_mwei);
    let init_rate_bump          = ext_params.as_ref().map_or(0, |p| p.init_rate_bump);
    let total_fees_1e5          = ext_params.as_ref().map_or(0, |p| p.total_fees);

    // Prefer extension-decoded points: the 1inch API frequently returns empty points
    // for orders that DO have extension breakpoints, causing systematic underestimation.
    //
    // Extension timeDelta values are RELATIVE (each delta added to the previous point's
    // cumulative time).  API points are also relative.
    let points: Vec<AuctionPoint> = if let Some(ext) = ext_params.as_ref().filter(|e| !e.points.is_empty()) {
        let mut cum: u64 = 0;
        let mut pts = Vec::with_capacity(ext.points.len());
        for &(coeff, delta) in &ext.points {
            cum += u64::from(delta);
            if cum > auction_duration_secs {
                break;
            }
            pts.push(AuctionPoint {
                delay_secs: cum,
                amount: apply_rate_bump(taking_amount, u128::from(coeff)),
                rate_bump: coeff,
            });
        }
        pts
    } else {
        // Fall back to API points (relative delays, need accumulation, no rate_bump).
        let mut cum: u64 = 0;
        let mut pts = Vec::with_capacity(item.points.len());
        for p in &item.points {
            cum += p.delay;
            if cum >= auction_duration_secs {
                break;
            }
            pts.push(AuctionPoint {
                delay_secs: cum,
                amount: apply_rate_bump(taking_amount, p.coefficient),
                rate_bump: 0_u32,
            });
        }
        pts
    };

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
        auction_start_amount: apply_rate_bump(taking_amount, eff_initial_rate_bump),
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
        init_rate_bump,
        total_fees_1e5,
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
    /// Auction breakpoints: `(coefficient, timeDelta_seconds)`.
    ///
    /// **`timeDelta` is RELATIVE** to the previous point (or auction start for the first point).
    /// Cumulative delays are computed in `convert()` when populating `AuctionPoint.delay_secs`.
    points: Vec<(u32, u16)>,
    /// `integratorFee + resolverFee` encoded in the extension (units of 1e5 = 100%).
    total_fees: u32,
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
///   [69]     point count      (uint8)   → hex chars [138:140]
///   [70+]    N × (coeff uint24 + delay uint16) → hex chars [140 + N×10]
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
        return None;
    }

    // Parse auction breakpoints when present.
    // Each point: 6 hex chars (coeff uint24) + 4 hex chars (timeDelta_relative uint16).
    let (points, total_fees) = if raw.len() >= 140 {
        let count = usize::from(u8::from_str_radix(&raw[138..140], 16).unwrap_or(0));
        let mut pts = Vec::with_capacity(count);
        for i in 0..count {
            let base = 140 + i * 10;
            if raw.len() < base + 10 {
                break;
            }
            let Ok(coeff) = u32::from_str_radix(&raw[base..base + 6], 16) else {
                break;
            };
            let Ok(delta) = u16::from_str_radix(&raw[base + 6..base + 10], 16) else {
                break;
            };
            pts.push((coeff, delta));
        }

        // Fee data starts immediately after points: [138 + 2 + count*10 .. ]
        // Layout: integratorFee (uint16, 4 chars) | integratorShare (uint8, 2 chars)
        //         resolverFee (uint16, 4 chars)   | whitelistDiscount (uint8, 2 chars)
        let fee_base = 140 + count * 10;
        let fees = if raw.len() >= fee_base + 12 {
            let integrator_fee =
                u32::from_str_radix(&raw[fee_base..fee_base + 4], 16).unwrap_or(0);
            let resolver_fee =
                u32::from_str_radix(&raw[fee_base + 6..fee_base + 10], 16).unwrap_or(0);
            integrator_fee + resolver_fee
        } else {
            0
        };
        (pts, fees)
    } else {
        (Vec::new(), 0)
    };

    Some(ExtensionParams {
        start_time,
        duration,
        init_rate_bump,
        gas_bump_estimate,
        gas_price_estimate_mwei,
        points,
        total_fees,
    })
}

fn parse_iso_timestamp(s: &str) -> anyhow::Result<u64> {
    let dt = DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("not a valid RFC 3339 timestamp: {s:?}"))?;
    u64::try_from(dt.timestamp())
        .with_context(|| format!("negative unix timestamp in {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::expect_used, reason = "test assertion helper")]
    fn decode_extension_params_parses_two_points_and_fee() {
        // Synthetic: 32-byte header + 20-byte address + fixed fields + 2 points + fee bytes.
        // timeDelta values are RELATIVE (not cumulative).
        let ext = concat!(
            "0x",
            "0000000000000000000000000000000000000000000000000000000000000000", // LOP header
            "0000000000000000000000000000000000000000",                         // address
            "000001",   // gasBumpEstimate  = 1
            "00000001", // gasPriceEstimate = 1 Mwei
            "00000064", // startTime        = 100
            "000064",   // duration         = 100 s
            "000001",   // initialRateBump  = 1
            "02",           // 2 points
            "000100000A",   // coeff=256, timeDelta=10 (relative)
            "0000800064",   // coeff=128, timeDelta=100 (relative)
            "012c",         // integratorFee = 300 (in 1e5)
            "64",           // integratorShare = 100
            "0000",         // resolverFee = 0
            "64",           // whitelistDiscountNumerator = 100
        );
        let p = decode_extension_params(ext).expect("should decode");
        assert_eq!(p.start_time, 100);
        assert_eq!(p.duration, 100);
        assert_eq!(p.init_rate_bump, 1);
        assert_eq!(p.gas_bump_estimate, 1);
        assert_eq!(p.gas_price_estimate_mwei, 1);
        assert_eq!(p.points, vec![(256, 10), (128, 100)]);
        assert_eq!(p.total_fees, 300); // integratorFee=300 + resolverFee=0
    }

    #[test]
    #[expect(clippy::expect_used, reason = "test assertion helper")]
    fn decode_extension_params_zero_points() {
        let ext = concat!(
            "0x",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000",
            "000001", "00000001", "00000064", "000064", "000001",
            "00", // 0 points
        );
        let p = decode_extension_params(ext).expect("should decode");
        assert!(p.points.is_empty());
        assert_eq!(p.total_fees, 0);
    }

    #[test]
    fn decode_extension_params_too_short_returns_none() {
        assert!(decode_extension_params("0x00").is_none());
        assert!(decode_extension_params("0x").is_none());
    }
}
