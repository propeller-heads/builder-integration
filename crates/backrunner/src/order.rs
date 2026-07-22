use std::str::FromStr;

use alloy::primitives::{keccak256, Address, Signature, B256, U256};
use serde::{Deserialize, Serialize};

use crate::client::{address_to_u256, hex_to_bytes, LOP_V4};

/// Divisor for rate bumps stored in auction points and `init_rate_bump` (1e7 = 100%).
const RATE_BUMP_DIVISOR: u128 = 10_000_000;

/// Divisor for the integrator/resolver fee encoded in extensions (1e5 = 100%).
const FEE_DIVISOR: u128 = 100_000;

/// A single breakpoint on the Fusion auction curve.
///
/// `delay_secs` is CUMULATIVE from `auction_start_time` (not relative to the previous point).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuctionPoint {
    /// Seconds elapsed from `auction_start_time` at which this breakpoint applies.
    pub delay_secs: u64,
    /// Required `to_token` output amount at this breakpoint (= `apply_rate_bump(floor, rate_bump)`).
    pub amount: U256,
    /// Rate bump coefficient from the extension (units of 1e7).  Zero for API-decoded points.
    #[serde(default)]
    pub rate_bump: u32,
}

/// A live 1inch Fusion limit order with its Dutch auction parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionOrder {
    /// 1inch order hash (`0x`-prefixed hex) uniquely identifying this order.
    pub order_id: String,
    /// Maker asset address (0x-prefixed hex, lowercase).
    pub from_token: String,
    /// Taker asset address (0x-prefixed hex, lowercase).
    pub to_token: String,
    /// Sell amount in the smallest token unit.
    pub making_amount: U256,
    /// Required output at the very start of the auction (most favourable for the user).
    pub auction_start_amount: U256,
    /// Minimum acceptable output (floor, reached at `auction_duration_secs`).
    pub auction_end_amount: U256,
    /// Total auction length in seconds.
    pub auction_duration_secs: u64,
    /// Unix timestamp at which the auction opened.
    pub auction_start_time: u64,
    /// Piecewise-linear decay curve breakpoints (may be empty).
    pub points: Vec<AuctionPoint>,
    /// Human-readable symbol for `from_token`, as reported by the Fusion API (may be absent).
    pub from_token_symbol: Option<String>,
    /// Human-readable symbol for `to_token`, as reported by the Fusion API (may be absent).
    pub to_token_symbol: Option<String>,
    /// Decimals of `from_token`. Falls back to a hardcoded table, then 18, if the API omits it.
    pub from_token_decimals: u8,
    /// Decimals of `to_token`. Falls back to a hardcoded table, then 18, if the API omits it.
    pub to_token_decimals: u8,
    /// USD value of one whole `from_token` at order creation.
    pub from_token_usd_rate: f64,
    /// USD value of one whole `to_token` at order creation.
    pub to_token_usd_rate: f64,
    /// Extension gasBumpEstimate (uint24): factor to multiply by baseFee/gasPriceEst.
    ///
    /// On-chain: gasBump = gasBumpEstimate × `baseFee_wei` / (gasPriceEstimate × 10^6).
    /// Included in the taking-amount computation so the profitability check is accurate.
    pub gas_bump_estimate: u32,
    /// Extension gasPriceEstimate (uint32) in units of 10^6 wei (Mwei).
    pub gas_price_estimate_mwei: u32,
    /// Initial rate bump from the extension (units of 1e7, same as `AuctionPoint.rate_bump`).
    ///
    /// Required for the exact on-chain `getTakingAmount` formula. Zero when decoded from API only.
    #[serde(default)]
    pub init_rate_bump: u32,
    /// Combined integrator + resolver fee from the extension (units of 1e5).
    ///
    /// Applied as `ceil(floor × (1e5 + fee) / 1e5)` before the rate-bump multiplier.
    /// Zero when no fee is present or when decoded from API only.
    #[serde(default)]
    pub total_fees_1e5: u32,
    /// EIP-712 maker signature (65 bytes, `0x`-prefixed hex).
    pub signature: String,
    /// Fusion Dutch-auction extension bytes (`0x`-prefixed hex, may be `"0x"` if empty).
    pub extension: String,
    /// Order salt (matches the on-chain order's `salt` field).
    pub salt: String,
    /// Maker EOA address (`0x`-prefixed hex).
    pub maker_address: String,
    /// Receiver address (`0x`-prefixed hex).
    pub receiver_address: String,
    /// Encoded maker traits as decimal string.
    pub maker_traits: String,
}

/// Returns true for GTC (Good Till Cancelled) limit orders that carry no Dutch auction premium.
///
/// GTC orders have no piecewise decay and an unusually long window (>1 hour). They produce
/// noise in solvability analysis and are skipped during quoting.
pub fn is_gtc_order(order: &FusionOrder) -> bool {
    const GTC_DURATION_THRESHOLD_SECS: u64 = 3_600;
    order.auction_start_amount <= order.auction_end_amount
        && order.auction_duration_secs > GTC_DURATION_THRESHOLD_SECS
}

/// EIP-712 domain name for the deployed LOP v4 contract.
///
/// NOT "1inch Limit Order Protocol" — the contract's constructor-set EIP-712 domain was
/// verified live via `eip712Domain()` (EIP-5267) on the deployed contract
/// (0x111111125421cA6dc452d289314280a0f8842A65, chain 1): it returns name
/// `"1inch Aggregation Router"`, version `"6"`. 1inch's source on GitHub still shows
/// `EIP712("1inch Limit Order Protocol", "4")`, but that does not match what is actually
/// deployed at this address today.
const LOP_DOMAIN_NAME: &str = "1inch Aggregation Router";
/// EIP-712 domain version for the deployed LOP v4 contract. See [`LOP_DOMAIN_NAME`].
const LOP_DOMAIN_VERSION: &str = "6";
/// EIP-712 `Order` struct type string, exactly matching the deployed LOP v4 layout
/// (all address fields typed as `address`, NOT the packed `uint256` used by [`crate::abi`]'s
/// `IOrderMixin::Order` — that layout yields a different typehash and would silently reject
/// every real order).
const ORDER_TYPE_STRING: &[u8] = b"Order(uint256 salt,address maker,address receiver,address \
makerAsset,address takerAsset,uint256 makingAmount,uint256 takingAmount,uint256 makerTraits)";

/// Result of independently verifying a [`FusionOrder`]'s maker signature offchain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MakerSigCheck {
    /// The signature recovers to `maker_address`.
    Verified,
    /// The signature is well-formed but recovers to a different address than `maker_address`.
    Mismatch {
        /// The address recovered from the signature over the order digest.
        recovered: Address,
    },
    /// The signature cannot be verified offchain: it is not a 65-byte `r||s||v` EOA
    /// signature (most likely an ERC-1271 contract-maker signature), or the order's
    /// fields could not be parsed. Callers should defer to on-chain validation.
    Unsupported,
}

impl FusionOrder {
    /// Computes the LOP v4 EIP-712 order digest: `keccak256(0x1901 ++ domainSeparator ++
    /// hashStruct(order))`.
    ///
    /// This is the exact value the 1inch API reports as `order_hash`, and the value
    /// `ecrecover`/`isValidSignature` is checked against on-chain. Reused to
    /// independently authenticate a Fusion order's maker signature offchain.
    pub fn order_digest(&self, chain_id: u64) -> anyhow::Result<B256> {
        let domain_separator = lop_domain_separator(chain_id);
        let struct_hash = self.order_struct_hash()?;

        let mut preimage = Vec::with_capacity(2 + 32 + 32);
        preimage.extend_from_slice(&[0x19, 0x01]);
        preimage.extend_from_slice(domain_separator.as_slice());
        preimage.extend_from_slice(struct_hash.as_slice());
        Ok(keccak256(&preimage))
    }

    /// `hashStruct(order) = keccak256(ORDER_TYPEHASH ++ 8×32-byte field words)`.
    fn order_struct_hash(&self) -> anyhow::Result<B256> {
        let salt = U256::from_str(&self.salt)
            .map_err(|e| anyhow::anyhow!("invalid order salt {:?}: {e}", self.salt))?;
        let maker_traits = U256::from_str(&self.maker_traits)
            .map_err(|e| anyhow::anyhow!("invalid maker_traits {:?}: {e}", self.maker_traits))?;
        let maker = address_to_u256(&self.maker_address)?;
        let receiver = address_to_u256(&self.receiver_address)?;
        let maker_asset = address_to_u256(&self.from_token)?;
        let taker_asset = address_to_u256(&self.to_token)?;

        let fields = [
            salt,
            maker,
            receiver,
            maker_asset,
            taker_asset,
            self.making_amount,
            self.auction_end_amount, // LOP takingAmount == our Dutch-auction floor
            maker_traits,
        ];

        let mut preimage = Vec::with_capacity(32 * (1 + fields.len()));
        preimage.extend_from_slice(keccak256(ORDER_TYPE_STRING).as_slice());
        for field in fields {
            preimage.extend_from_slice(&field.to_be_bytes::<32>());
        }
        Ok(keccak256(&preimage))
    }

    /// Independently verifies the maker's EIP-712 signature over this order.
    ///
    /// Only 65-byte `r||s||v` EOA signatures can be checked offchain (`v` accepted in
    /// either `{27,28}` or `{0,1}` form). Non-65-byte signatures — almost always an
    /// ERC-1271 contract-maker signature — return [`MakerSigCheck::Unsupported`];
    /// callers should defer to on-chain `isValidSignature` validation for those.
    #[must_use]
    pub fn verify_maker_signature(&self, chain_id: u64) -> MakerSigCheck {
        let Ok(sig_bytes) = hex_to_bytes(&self.signature) else {
            return MakerSigCheck::Unsupported;
        };
        if sig_bytes.len() != 65 {
            return MakerSigCheck::Unsupported;
        }
        let Ok(digest) = self.order_digest(chain_id) else {
            return MakerSigCheck::Unsupported;
        };
        let Ok(signature) = Signature::from_raw(&sig_bytes) else {
            return MakerSigCheck::Unsupported;
        };
        let Ok(recovered) = signature.recover_address_from_prehash(&digest) else {
            return MakerSigCheck::Unsupported;
        };
        let Ok(maker) = Address::from_str(&self.maker_address) else {
            return MakerSigCheck::Unsupported;
        };
        if recovered == maker {
            MakerSigCheck::Verified
        } else {
            MakerSigCheck::Mismatch { recovered }
        }
    }
}

/// Computes the EIP-712 domain separator for the LOP v4 contract on `chain_id`.
fn lop_domain_separator(chain_id: u64) -> B256 {
    let domain_typehash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let name_hash = keccak256(LOP_DOMAIN_NAME.as_bytes());
    let version_hash = keccak256(LOP_DOMAIN_VERSION.as_bytes());

    let mut preimage = Vec::with_capacity(32 * 5);
    preimage.extend_from_slice(domain_typehash.as_slice());
    preimage.extend_from_slice(name_hash.as_slice());
    preimage.extend_from_slice(version_hash.as_slice());
    preimage.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    preimage.extend_from_slice(&address_word(LOP_V4));
    keccak256(&preimage)
}

/// Left-pads an address into its 32-byte EIP-712 ABI word.
fn address_word(address: Address) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    word
}

/// Computes the exact on-chain taking amount matching `SimpleSettlement.getTakingAmount`:
///
/// ```text
/// auctionBump = interpolate rate bumps at unix_ts (piecewise-linear on coefficients)
/// gasBump     = gasBumpEstimate × baseFee / (gasPriceEstimate × 10^6)
/// rateBump    = max(0, auctionBump − gasBump)          ← gas subtracts from rate
/// withFees    = ceil(floor × (1e5 + totalFees) / 1e5)
/// final       = ceil(withFees × (1e7 + rateBump) / 1e7)
/// ```
///
/// Returns `None` when the auction has not started or has expired.
///
/// Falls back to `amount_at_timestamp` semantics if `init_rate_bump == 0` (API-only decode),
/// in which case `total_fees_1e5` is still applied if non-zero.
pub fn onchain_taking_amount(order: &FusionOrder, unix_ts: u64, base_fee_wei: u64) -> Option<U256> {
    let elapsed = elapsed_secs(order, unix_ts)?;
    let auction_bump = interpolate_rate_bump(order, elapsed);
    let gas_bump = compute_gas_bump(order, base_fee_wei);
    let rate_bump = auction_bump.saturating_sub(gas_bump);
    let with_fees = apply_fee_bump(order.auction_end_amount, u128::from(order.total_fees_1e5));
    Some(apply_rate_bump_order(with_fees, rate_bump))
}

/// Interpolates the auction rate bump at `elapsed` seconds using `order.points[].rate_bump`.
///
/// Matches the on-chain `_getAuctionBump` piecewise-linear interpolation on rate-bump
/// coefficients (units of 1e7).  Returns 0 when the auction has expired.
fn interpolate_rate_bump(order: &FusionOrder, elapsed: u64) -> u128 {
    let mut current_t: u64 = 0;
    let mut current_bump = u128::from(order.init_rate_bump);

    for point in &order.points {
        if elapsed <= point.delay_secs {
            return interp_bump(current_t, current_bump, point.delay_secs, u128::from(point.rate_bump), elapsed);
        }
        current_t = point.delay_secs;
        current_bump = u128::from(point.rate_bump);
    }

    // Past all explicit points: decay linearly from last point to 0 at duration end.
    let duration = order.auction_duration_secs;
    if duration > current_t {
        interp_bump(current_t, current_bump, duration, 0, elapsed)
    } else {
        0
    }
}

/// Linear interpolation between two (time, bump) pairs — floor division, matching Solidity.
fn interp_bump(t0: u64, b0: u128, t1: u64, b1: u128, t: u64) -> u128 {
    let span = u128::from(t1.saturating_sub(t0));
    if span == 0 {
        return b0;
    }
    let elapsed_in = u128::from(t.saturating_sub(t0));
    let remaining = u128::from(t1.saturating_sub(t));
    (b0.saturating_mul(remaining).saturating_add(b1.saturating_mul(elapsed_in))) / span
}

/// `base + ceil(base × fees / FEE_DIVISOR)` — fee applied with 1e5 denominator.
fn apply_fee_bump(base: U256, fees: u128) -> U256 {
    let divisor = U256::from(FEE_DIVISOR);
    let increment = base
        .saturating_mul(U256::from(fees))
        .saturating_add(divisor - U256::ONE)
        / divisor;
    base.saturating_add(increment)
}

/// `base + ceil(base × bump / RATE_BUMP_DIVISOR)` — rate bump with 1e7 denominator.
fn apply_rate_bump_order(base: U256, bump: u128) -> U256 {
    let divisor = U256::from(RATE_BUMP_DIVISOR);
    let increment = base
        .saturating_mul(U256::from(bump))
        .saturating_add(divisor - U256::ONE)
        / divisor;
    base.saturating_add(increment)
}

/// Computes the gas-bump rate for an order given the pending block's base fee.
///
/// On-chain formula: `gasBump = gasBumpEstimate × baseFee_wei / (gasPriceEstimate × 10^6)`
/// The gas bump is SUBTRACTED from the auction bump in `onchain_taking_amount`.
///
/// Returns 0 when the order has no gas-bump configured (`gas_price_estimate_mwei == 0`).
pub fn compute_gas_bump(order: &FusionOrder, base_fee_wei: u64) -> u128 {
    let gas_price_scaled = u64::from(order.gas_price_estimate_mwei) * 1_000_000;
    if gas_price_scaled == 0 {
        return 0;
    }
    u128::from(order.gas_bump_estimate)
        .saturating_mul(u128::from(base_fee_wei))
        / u128::from(gas_price_scaled)
}

/// Returns the minimum required output amount A(t) at `unix_ts`.
///
/// Returns `Some` when `unix_ts` is within the half-open window
/// `[auction_start_time, auction_start_time + auction_duration_secs)`.
/// Returns `None` when the order has expired or not yet started.
pub fn amount_at_timestamp(order: &FusionOrder, unix_ts: u64) -> Option<U256> {
    let elapsed = elapsed_secs(order, unix_ts)?;
    let (t0, a0, t1, a1) = find_segment(order, elapsed);
    Some(interpolate(t0, a0, t1, a1, elapsed))
}

fn elapsed_secs(order: &FusionOrder, unix_ts: u64) -> Option<u64> {
    if unix_ts < order.auction_start_time {
        return None;
    }
    let elapsed = unix_ts - order.auction_start_time;
    if elapsed >= order.auction_duration_secs {
        return None;
    }
    Some(elapsed)
}

fn find_segment(order: &FusionOrder, elapsed: u64) -> (u64, U256, u64, U256) {
    let mut seg_start_t: u64 = 0;
    let mut seg_start_a: U256 = order.auction_start_amount;

    for point in &order.points {
        if elapsed < point.delay_secs {
            return (seg_start_t, seg_start_a, point.delay_secs, point.amount);
        }
        seg_start_t = point.delay_secs;
        seg_start_a = point.amount;
    }

    (seg_start_t, seg_start_a, order.auction_duration_secs, order.auction_end_amount)
}

fn interpolate(t0: u64, a0: U256, t1: u64, a1: U256, t: u64) -> U256 {
    let span = U256::from(t1.saturating_sub(t0));
    if span.is_zero() {
        return a0;
    }
    let elapsed_in_segment = U256::from(t.saturating_sub(t0));
    let decay = a0.saturating_sub(a1).saturating_mul(elapsed_in_segment) / span;
    a0.saturating_sub(decay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_order() -> FusionOrder {
        FusionOrder {
            order_id: "test".into(),
            from_token: "0xa".into(),
            to_token: "0xb".into(),
            making_amount: U256::from(1_000u64),
            auction_start_amount: U256::from(1_000u64),
            auction_end_amount: U256::from(800u64),
            auction_duration_secs: 200,
            auction_start_time: 1_000,
            points: vec![],
            from_token_symbol: None,
            to_token_symbol: None,
            from_token_decimals: 18,
            to_token_decimals: 18,
            from_token_usd_rate: 1.0,
            to_token_usd_rate: 1.0,
            gas_bump_estimate: 0,
            gas_price_estimate_mwei: 0,
            init_rate_bump: 0,
            total_fees_1e5: 0,
            signature: "0x00".to_string(),
            extension: "0x".to_string(),
            salt: "0".to_string(),
            maker_address: "0x0000000000000000000000000000000000000000".to_string(),
            receiver_address: "0x0000000000000000000000000000000000000000".to_string(),
            maker_traits: "0".to_string(),
        }
    }

    #[test]
    fn before_start_returns_none() {
        assert_eq!(amount_at_timestamp(&simple_order(), 999), None);
    }

    #[test]
    fn after_end_returns_none() {
        assert_eq!(amount_at_timestamp(&simple_order(), 1_200), None);
    }

    #[test]
    fn at_start_returns_start_amount() {
        assert_eq!(amount_at_timestamp(&simple_order(), 1_000), Some(U256::from(1_000u64)));
    }

    #[test]
    fn linear_midpoint() {
        // elapsed = 100/200 → halfway between 1000 and 800 → 900
        assert_eq!(amount_at_timestamp(&simple_order(), 1_100), Some(U256::from(900u64)));
    }

    #[test]
    fn two_segment_breakpoint() {
        let order = FusionOrder {
            points: vec![AuctionPoint { delay_secs: 100, amount: U256::from(900u64), rate_bump: 0 }],
            ..simple_order()
        };
        assert_eq!(amount_at_timestamp(&order, 1_050), Some(U256::from(950u64)));
        assert_eq!(amount_at_timestamp(&order, 1_100), Some(U256::from(900u64)));
        assert_eq!(amount_at_timestamp(&order, 1_150), Some(U256::from(850u64)));
    }

    #[test]
    fn is_gtc_detects_flat_long_order() {
        let gtc = FusionOrder {
            auction_start_amount: U256::from(1_000u64),
            auction_end_amount: U256::from(1_000u64),
            auction_duration_secs: 7_200,
            ..simple_order()
        };
        assert!(is_gtc_order(&gtc));
    }

    #[test]
    fn is_gtc_passes_normal_dutch_auction() {
        assert!(!is_gtc_order(&simple_order()));
    }

    #[test]
    fn integer_arithmetic_on_wei_scale() {
        let order = FusionOrder {
            making_amount: U256::from(1_000_000_000_000_000_000u128),
            auction_start_amount: U256::from(1_001_000_000_000_000_000u128),
            auction_end_amount: U256::from(1_000_000_000_000_000_000u128),
            auction_duration_secs: 180,
            auction_start_time: 1_000,
            ..simple_order()
        };
        assert_eq!(
            amount_at_timestamp(&order, 1_090),
            Some(U256::from(1_000_500_000_000_000_000u128))
        );
    }

    // ── Real-order regression tests ──────────────────────────────────────────
    //
    // Parameters decoded from live extension bytes observed in smoke run 3
    // (block 25230553, June 2026).  These anchor the formula to on-chain ground
    // truth and guard against silent arithmetic regressions.

    /// FABA→WETH order captured at block 25230553.
    ///
    /// Extension params decoded from `TakingAmountData`:
    /// - `startTime=1780413608`, `duration=360s`, `initialRateBump=695823`
    /// - `floor=13_632_559_937_529_719` (order.takingAmount)
    /// - `start_amount=14_581_144_812_870_894`
    ///
    /// Ground truth: smoke run 3 log with `base_fee=0` → `gas_bump=0`.
    /// - `taking_estimate=14573239938909718` at `pending_ts=1780413611` (elapsed=3s)
    fn faba_order() -> FusionOrder {
        FusionOrder {
            order_id: "0x9d7a1175ffd8e3b62e32c68657a1fa7bc08a7d8f07161a31ce9ce14560448c54".into(),
            from_token: "0xfaba6f8e4a5e8ab82f62fe7c39859fa577269be3".into(),
            to_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            making_amount: U256::from(307_671_843_799_523_540u128),
            auction_start_amount: U256::from(14_581_144_812_870_894u128),
            auction_end_amount:   U256::from(13_632_559_937_529_719u128),
            auction_duration_secs: 360,
            auction_start_time:    1_780_413_608,
            points: vec![],
            from_token_symbol: Some("FABA".into()),
            to_token_symbol:   Some("WETH".into()),
            from_token_decimals: 18,
            to_token_decimals:   18,
            from_token_usd_rate: 0.0,
            to_token_usd_rate:   0.0,
            gas_bump_estimate:       343_213,
            gas_price_estimate_mwei:   2_076,
            init_rate_bump: 0,
            total_fees_1e5: 0,
            signature: "0x".into(),
            extension: "0x".into(),
            salt: "0".into(),
            maker_address:    "0x0000000000000000000000000000000000000000".into(),
            receiver_address: "0x0000000000000000000000000000000000000000".into(),
            maker_traits: "0".into(),
        }
    }

    #[test]
    fn faba_auction_price_at_elapsed_3s() {
        // pending_ts = startTime + 3 = 1_780_413_611
        assert_eq!(
            amount_at_timestamp(&faba_order(), 1_780_413_611),
            Some(U256::from(14_573_239_938_909_718u128)),
        );
    }

    #[test]
    fn faba_before_auction_start_returns_none() {
        // confirmed block 25230552 had timestamp=1780413599, which is 9s BEFORE startTime.
        // This replicates what the on-chain eth_call at "latest" would see — the auction
        // hasn't started, so the extension should be called with timestamp override instead.
        assert_eq!(amount_at_timestamp(&faba_order(), 1_780_413_599), None);
    }

    #[test]
    fn faba_gas_bump_at_base_fee() {
        // Base fee from confirmed block 25230552: 1_871_798_811 wei.
        let bump = compute_gas_bump(&faba_order(), 1_871_798_811);
        assert_eq!(bump, 309_453);
        let floor = faba_order().auction_end_amount;
        let taking = floor
            .saturating_mul(U256::from(bump))
            .saturating_add(U256::from(9_999_999u64))
            / U256::from(10_000_000u64);
        assert_eq!(taking, U256::from(421_863_657_034_839u128));
    }

    #[test]
    #[expect(clippy::expect_used, reason = "test assertion helper")]
    fn faba_full_estimate_at_elapsed_3s() {
        // Combined auction price + gas bump — the value compared against amount_out.
        let order = faba_order();
        let block_ts = 1_780_413_611_u64;
        let base_fee = 1_871_798_811_u64;

        let auction_price = amount_at_timestamp(&order, block_ts).expect("within auction window");
        let gas_bump      = compute_gas_bump(&order, base_fee);
        let gas_bump_tak  = order.auction_end_amount
            .saturating_mul(U256::from(gas_bump))
            .saturating_add(U256::from(9_999_999u64))
            / U256::from(10_000_000u64);
        assert_eq!(auction_price + gas_bump_tak, U256::from(14_995_103_595_944_557u128));
    }

    /// WETH→USDT order from `encode_test.rs` (block 25222660).
    ///
    /// Extension params decoded from `TakingAmountData`:
    /// - `startTime=1780318371`, `duration=180s`, `initialRateBump=53347`
    /// - `floor=1_327_889_927`, `start_amount=1_334_973_822`
    /// - `gasBumpEstimate=3080`, `gasPriceEstimate=336 Mwei`
    fn usdt_order() -> FusionOrder {
        FusionOrder {
            order_id: "0xddc5239bef2a6f7afc8967384e209ec5548215abda64e5a68e89e7e0741f2090".into(),
            from_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            to_token:   "0xdac17f958d2ee523a2206206994597c13d831ec7".into(),
            making_amount: U256::from(671_300_000_000_000_000u128),
            auction_start_amount: U256::from(1_334_973_822u64),
            auction_end_amount:   U256::from(1_327_889_927u64),
            auction_duration_secs: 180,
            auction_start_time:    1_780_318_371,
            points: vec![],
            from_token_symbol: Some("WETH".into()),
            to_token_symbol:   Some("USDT".into()),
            from_token_decimals: 18,
            to_token_decimals:    6,
            from_token_usd_rate: 0.0,
            to_token_usd_rate:   0.0,
            gas_bump_estimate:       3_080,
            gas_price_estimate_mwei:   336,
            init_rate_bump: 0,
            total_fees_1e5: 0,
            signature: "0x".into(),
            extension: "0x".into(),
            salt: "0".into(),
            maker_address:    "0x0000000000000000000000000000000000000000".into(),
            receiver_address: "0x0000000000000000000000000000000000000000".into(),
            maker_traits: "0".into(),
        }
    }

    #[test]
    fn usdt_auction_at_start() {
        assert_eq!(
            amount_at_timestamp(&usdt_order(), 1_780_318_371),
            Some(U256::from(1_334_973_822u64)),
        );
    }

    #[test]
    fn usdt_auction_at_elapsed_27s() {
        // elapsed=27: decay = (1334973822 - 1327889927) * 27 / 180 = 1062584
        // result = 1334973822 - 1062584 = 1333911238
        assert_eq!(
            amount_at_timestamp(&usdt_order(), 1_780_318_398),
            Some(U256::from(1_333_911_238u64)),
        );
    }

    #[test]
    fn usdt_auction_at_elapsed_90s() {
        assert_eq!(
            amount_at_timestamp(&usdt_order(), 1_780_318_461),
            Some(U256::from(1_331_431_875u64)),
        );
    }

    #[test]
    fn usdt_auction_expired_returns_none() {
        assert_eq!(amount_at_timestamp(&usdt_order(), 1_780_318_551), None);
    }

    // ── Smoke run 4 — UNI→USDC order, block 25230904 (eth_call SUCCESS) ──────
    //
    // Order 0x24d971ccc1964e91b28518779ed29cbcb4496849fd463791b99645f9d01a0c69
    //   makerAsset: UNI  (0x1f9840a85d5af5bf1d1762f925bdaddc4201f984)
    //   takerAsset: USDC (0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48)
    //   floor=60_227_591, start_amount=61_805_862
    //   startTime=1_780_417_845, duration=360s
    //   gasBumpEstimate=126_727, gasPriceEstimate=1_433 Mwei
    //
    // Extension has TWO auction points (API returns 0 — discrepancy):
    //   t=24s:  coeff=251_799 → amount=61_744_116
    //   t=360s: coeff=126_727 → amount=60_990_838  (≈ floor + gas-at-reference-price)
    //
    // Ground truth from debug trace (block 25230904, confirmed SUCCESS):
    //   onchain_taking = 63_030_547  (pending_ts=1_780_417_847, base_fee=1_269_763_641)
    //
    // NOTE: our off-chain estimate (62_473_396) is lower than onchain (63_030_547).
    // The ~535k gap is a resolver-tier fee encoded in the extension's third section
    // (not yet decoded off-chain).  The `query_onchain_taking_amount` pre-check
    // handles this correctly; these tests verify the auction-curve arithmetic only.

    fn uni_order_no_pts() -> FusionOrder {
        FusionOrder {
            order_id: "0x24d971ccc1964e91b28518779ed29cbcb4496849fd463791b99645f9d01a0c69".into(),
            from_token: "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984".into(),
            to_token:   "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            making_amount:        U256::from(22_379_390_100_267_360_763u128),
            auction_start_amount: U256::from(61_805_862u64),
            auction_end_amount:   U256::from(60_227_591u64),
            auction_duration_secs: 360,
            auction_start_time:    1_780_417_845,
            points: vec![],
            from_token_symbol:  Some("UNI".into()),
            to_token_symbol:    Some("USDC".into()),
            from_token_decimals: 18,
            to_token_decimals:    6,
            from_token_usd_rate: 0.0,
            to_token_usd_rate:   0.0,
            gas_bump_estimate:        126_727,
            gas_price_estimate_mwei:    1_433,
            init_rate_bump: 0,
            total_fees_1e5: 0,
            signature:        "0x".into(),
            extension:        "0x".into(),
            salt:             "0".into(),
            maker_address:    "0x0000000000000000000000000000000000000000".into(),
            receiver_address: "0x0000000000000000000000000000000000000000".into(),
            maker_traits:     "0".into(),
        }
    }

    fn uni_order_with_pts() -> FusionOrder {
        FusionOrder {
            points: vec![
                AuctionPoint { delay_secs: 24,  amount: U256::from(61_744_116u64), rate_bump: 0 },
                AuctionPoint { delay_secs: 360, amount: U256::from(60_990_838u64), rate_bump: 0 },
            ],
            ..uni_order_no_pts()
        }
    }

    // ── Smoke run 4 — USDC→floor order 0x1dca545a (VERIFIED against on-chain) ──────────
    //
    // Order: 0x1dca545afebf78140bb8fc7807401cc57f4cec36f7cf6d3f7e8f1e7e535ff3c6
    //   floor = 584_777_961
    //   startTime = 1_780_417_052, duration = 180s
    //   gasBumpEstimate = 126, gasPriceEstimate = 2501 Mwei
    //   initRateBump = 62_126 (gives start_amount = 588_410_953 = apply_rate_bump(floor, 62126))
    //   totalFees = 300 (resolverFee=300 in 1e5 units = 0.3%)
    //
    // Extension points (RELATIVE timeDelta):
    //   coeff=50377, timeDelta=60  → cumulative t=60
    //   coeff=50236, timeDelta=84  → cumulative t=60+84=144
    //   coeff=126,   timeDelta=36  → cumulative t=144+36=180 (=duration)
    //
    // Ground truth at block_ts=1_780_417_175, base_fee=2_310_734_453:
    //   elapsed=123, gasBump=116, auctionBump=50_271, rateBump=50_155
    //   withFees=586_532_295, onchain_taking=589_474_048
    fn dca_order() -> FusionOrder {
        FusionOrder {
            order_id: "0x1dca545afebf78140bb8fc7807401cc57f4cec36f7cf6d3f7e8f1e7e535ff3c6".into(),
            from_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            to_token:   "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            making_amount: U256::from(100_000_000_000_000_000u128),
            auction_start_amount: U256::from(588_410_953u64),
            auction_end_amount:   U256::from(584_777_961u64),
            auction_duration_secs: 180,
            auction_start_time:    1_780_417_052,
            points: vec![
                AuctionPoint { delay_secs:  60, amount: U256::from(587_723_897u64), rate_bump: 50_377 },
                AuctionPoint { delay_secs: 144, amount: U256::from(587_715_642u64), rate_bump: 50_236 },
                AuctionPoint { delay_secs: 180, amount: U256::from(584_785_334u64), rate_bump:    126 },
            ],
            from_token_symbol: Some("WETH".into()),
            to_token_symbol:   Some("USDC".into()),
            from_token_decimals: 18,
            to_token_decimals:    6,
            from_token_usd_rate: 0.0,
            to_token_usd_rate:   0.0,
            gas_bump_estimate:       126,
            gas_price_estimate_mwei: 2_501,
            init_rate_bump:  62_126,
            total_fees_1e5:    300,
            signature:        "0x".into(),
            extension:        "0x".into(),
            salt:             "0".into(),
            maker_address:    "0x0000000000000000000000000000000000000000".into(),
            receiver_address: "0x0000000000000000000000000000000000000000".into(),
            maker_traits:     "0".into(),
        }
    }

    #[test]
    fn uni_gas_bump_at_base_fee() {
        // base_fee = 1_269_763_641 from block 25230903 (confirmed parent of 25230904)
        // gas_bump = 126_727 * 1_269_763_641 / (1_433 * 1_000_000) = 112_291
        let bump = compute_gas_bump(&uni_order_no_pts(), 1_269_763_641);
        assert_eq!(bump, 112_291);
    }

    #[test]
    fn uni_price_at_elapsed_2s_no_points() {
        // Simple linear (as the 1inch API returns no points for this order).
        // elapsed=2: decay = (61_805_862 - 60_227_591) * 2 / 360 = 8_768 (floor div)
        assert_eq!(
            amount_at_timestamp(&uni_order_no_pts(), 1_780_417_847),
            Some(U256::from(61_797_094u64)),
        );
    }

    #[test]
    fn uni_price_at_elapsed_2s_with_ext_points() {
        // Piecewise with extension-decoded points (segment 0..24s).
        // decay = (61_805_862 - 61_744_116) * 2 / 24 = 5_145 (floor div)
        assert_eq!(
            amount_at_timestamp(&uni_order_with_pts(), 1_780_417_847),
            Some(U256::from(61_800_717u64)),
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "test assertion helper")]
    fn uni_full_estimate_no_points() {
        // Reproduces the logged taking_estimate=62_473_396 exactly.
        // smoke run 4: "auction price estimate ... taking_estimate=62473396"
        let order    = uni_order_no_pts();
        let block_ts = 1_780_417_847_u64;
        let base_fee = 1_269_763_641_u64;

        let price    = amount_at_timestamp(&order, block_ts).expect("within auction window");
        let gas_bump = compute_gas_bump(&order, base_fee);
        let gas_bump_taking = order.auction_end_amount
            .saturating_mul(U256::from(gas_bump))
            .saturating_add(U256::from(9_999_999u64))
            / U256::from(10_000_000u64);
        assert_eq!(price + gas_bump_taking, U256::from(62_473_396u64));
    }

    #[test]
    fn uni_before_start_returns_none() {
        // 1s before auction start
        assert_eq!(amount_at_timestamp(&uni_order_no_pts(), 1_780_417_844), None);
    }

    #[test]
    fn uni_expired_returns_none() {
        // At or after auction end (start + 360s)
        assert_eq!(
            amount_at_timestamp(&uni_order_no_pts(), 1_780_417_845 + 360),
            None,
        );
    }

    // ── onchain_taking_amount tests — exact match against query_onchain_taking_amount ──

    #[test]
    fn dca_gas_bump() {
        // 126 * 2_310_734_453 / (2501 * 1_000_000) = 116
        assert_eq!(compute_gas_bump(&dca_order(), 2_310_734_453), 116);
    }

    #[test]
    fn dca_onchain_taking_matches_ground_truth() {
        // Verified: query_onchain_taking_amount returned 589_474_048 at this block.
        // Formula: auctionBump=50_271, gasBump=116, rateBump=50_155
        //   withFees = ceil(584_777_961 * 100_300 / 100_000) = 586_532_295
        //   final    = ceil(586_532_295 * 10_050_155 / 10_000_000) = 589_474_048
        assert_eq!(
            onchain_taking_amount(&dca_order(), 1_780_417_175, 2_310_734_453),
            Some(U256::from(589_474_048u64)),
        );
    }

    #[test]
    fn dca_before_start_returns_none() {
        assert_eq!(onchain_taking_amount(&dca_order(), 1_780_417_051, 2_310_734_453), None);
    }

    #[test]
    fn dca_expired_returns_none() {
        assert_eq!(onchain_taking_amount(&dca_order(), 1_780_417_052 + 180, 2_310_734_453), None);
    }

    // ── EIP-712 order digest & maker-signature verification ───────────────────────────
    //
    // Ground truth: the real WETH→USDT order from `encode_test.rs`, filled on mainnet at
    // block 25222660 by tx 0x95fe1ab933411f472d0e7cb0d38f60ccf223ceb514989f7d2cad1da61b7a81dd
    // (`settleOrders` → `fillContractOrderArgs`). Fetched live and cross-checked against this
    // repo's fixture: every field (salt, maker, receiver, assets, amounts, makerTraits,
    // 256-byte ERC-1271 signature) decodes byte-for-byte identically to `encode_test.rs`'s
    // `SIGNATURE_HEX`/`RawOrderFields`.
    //
    // Two corrections this uncovered, relative to the original task brief:
    // 1. `0xddc5239bef2a...` — labelled `order_id`/`order_hash` in this repo's existing
    //    fixtures (`encode_test.rs`, `usdt_order()` above) — is actually this order's
    //    **salt**, not its order hash. (It does satisfy the LOP `isValidExtension` low-160-bit
    //    extension-hash binding, which is what made this look plausible.) The genuine order
    //    hash, confirmed via the real `OrderFilled(bytes32 orderHash, uint256 remainingAmount)`
    //    log in that transaction's receipt, is
    //    `0x1de5a862905f24eb617987b00c9889b4b87244a0a867b4ba17877f4b0eada6b6`.
    // 2. The deployed contract's EIP-712 domain is NOT `("1inch Limit Order Protocol", "4")`
    //    as 1inch's current GitHub source suggests — a live `eip712Domain()` (EIP-5267) call
    //    to 0x111111125421cA6dc452d289314280a0f8842A65 on chain 1 returns
    //    `("1inch Aggregation Router", "6")`. See [`LOP_DOMAIN_NAME`].
    //
    // This order's maker (0xc7ae508d...) is an ERC-1271 smart-contract wallet — its
    // signature is 256 bytes (see `encode_test.rs`, and `abi.rs`'s choice of
    // `fillContractOrderArgs` for non-65-byte signatures). So `verify_maker_signature`
    // on this exact real order must return `Unsupported`, never `Verified`. There is no
    // real EOA-signed 1inch order fixture anywhere in this repo, so the EOA recovery
    // path is exercised with a synthetic signature (Anvil/Foundry's well-known default
    // test key #0) over this same real, ground-truthed digest.

    /// Real ERC-1271 WETH→USDT order, filled at block 25222660 (see module-level comment
    /// above for the transaction that filled it and how the fields were cross-checked).
    fn real_erc1271_usdt_order() -> FusionOrder {
        FusionOrder {
            order_id: "0x1de5a862905f24eb617987b00c9889b4b87244a0a867b4ba17877f4b0eada6b6".into(),
            from_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            to_token:   "0xdac17f958d2ee523a2206206994597c13d831ec7".into(),
            making_amount:        U256::from(671_300_000_000_000_000u128),
            auction_start_amount: U256::from(1_334_973_822u64),
            auction_end_amount:   U256::from(1_327_889_927u64), // LOP takingAmount / floor
            auction_duration_secs: 180,
            auction_start_time:    1_780_318_371,
            points: vec![],
            from_token_symbol: Some("WETH".into()),
            to_token_symbol:   Some("USDT".into()),
            from_token_decimals: 18,
            to_token_decimals:    6,
            from_token_usd_rate: 0.0,
            to_token_usd_rate:   0.0,
            gas_bump_estimate:       3_080,
            gas_price_estimate_mwei:   336,
            init_rate_bump: 0,
            total_fees_1e5: 0,
            // Real 256-byte ERC-1271 signature (concat of encode_test.rs::SIGNATURE_HEX).
            signature: concat!(
                "0x",
                "ddc5239bef2a6f7afc8967384e209ec5548215abda64e5a68e89e7e0741f2090",
                "000000000000000000000000d27cc478689bea4dafe2ab7e486944d775e539a3",
                "000000000000000000000000399740157391a9f1bf4e9921a8834f9bc8f2678e",
                "000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "000000000000000000000000dac17f958d2ee523a2206206994597c13d831ec7",
                "0000000000000000000000000000000000000000000000000950efcb15b84000",
                "000000000000000000000000000000000000000000000000000000004f25fe07",
                "8a00000000000000000000001254000016006a1d816300000000000000000000",
            )
            .into(),
            extension: "0x".into(),
            // Decimal(0xddc5239bef2a6f7afc8967384e209ec5548215abda64e5a68e89e7e0741f2090)
            salt: "100309454173764179270272824781866126838468984213009708139965346861068851290256"
                .into(),
            maker_address:    "0xc7ae508ddc86d6acfeb80c3f0e972d1a22bacaad".into(),
            receiver_address: "0x399740157391a9f1bf4e9921a8834f9bc8f2678e".into(),
            // Decimal(0x8a00000000000000000000001254000016006a1d816300000000000000000000)
            maker_traits: "62419173104490761595518734106350460423654101960782298241773079837553102684160"
                .into(),
        }
    }

    #[test]
    #[expect(clippy::expect_used, reason = "test assertion helper")]
    fn order_digest_matches_real_order_hash() {
        let order = real_erc1271_usdt_order();
        let expected: B256 = "0x1de5a862905f24eb617987b00c9889b4b87244a0a867b4ba17877f4b0eada6b6"
            .parse()
            .expect("valid B256 literal");
        assert_eq!(
            order.order_digest(1).expect("well-formed order fields"),
            expected
        );
    }

    #[test]
    fn verify_maker_signature_unsupported_for_erc1271_order() {
        // Real fixture's signature is 256 bytes (ERC-1271 contract maker) — cannot be
        // ecrecover-verified offchain, regardless of how the digest checks out.
        let order = real_erc1271_usdt_order();
        assert_eq!(order.verify_maker_signature(1), MakerSigCheck::Unsupported);
    }

    /// Builds a copy of the real order with `maker_address` replaced by Anvil/Foundry's
    /// well-known default test account #0, and a fresh 65-byte EOA signature over the
    /// resulting (maker-consistent) digest — `maker_address` MUST be set before the
    /// digest is computed, since `maker` is itself part of the struct hash.
    #[expect(clippy::expect_used, reason = "test fixture helper")]
    fn synthetic_eoa_order() -> FusionOrder {
        use alloy::signers::local::PrivateKeySigner;
        use alloy::signers::SignerSync;

        let signer: PrivateKeySigner =
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
                .parse()
                .expect("valid test private key");

        let mut order = real_erc1271_usdt_order();
        order.maker_address = format!("{:#x}", signer.address());

        let digest = order.order_digest(1).expect("well-formed order fields");
        let signature = signer
            .sign_hash_sync(&digest)
            .expect("signing a 32-byte digest cannot fail");
        order.signature = format!("0x{}", hex::encode(signature.as_bytes()));
        order
    }

    #[test]
    fn verify_maker_signature_verified_for_synthetic_eoa_order() {
        // Same real, ground-truthed order fields, but with a synthetic 65-byte EOA
        // signature over the same digest mechanism validated in
        // `order_digest_matches_real_order_hash`, and `maker_address` set to that
        // signer — proving the ecrecover path end to end.
        let order = synthetic_eoa_order();
        assert_eq!(order.verify_maker_signature(1), MakerSigCheck::Verified);
    }

    #[test]
    fn verify_maker_signature_mismatch_when_amount_tampered() {
        let mut order = synthetic_eoa_order();
        let maker = order.maker_address.clone();

        // Tamper a field AFTER signing: the signature was computed over the original
        // digest, so the recomputed digest (and recovered signer) now differ.
        order.making_amount += U256::from(1u64);

        let MakerSigCheck::Mismatch { recovered } = order.verify_maker_signature(1) else {
            unreachable!("tampering the making_amount must change the digest and thus the check");
        };
        assert_ne!(format!("{recovered:#x}"), maker.to_lowercase());
    }
}
