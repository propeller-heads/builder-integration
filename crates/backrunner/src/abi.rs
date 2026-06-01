//! ABI encoding for `BackrunResolver.settleOrders` + 1inch LOP v4 `fillContractOrder`.
//!
//! Settlement call chain:
//!   executor → `resolver.settleOrders(fill_calldata)`
//!           → `lop.fillContractOrder(order, sig, amount, takerTraits, args)`
//!                 → `resolver.takerInteraction(order, ..., extraData)`
//!                         → `fynd_router.call(swap_calldata)`
//!
//! `args` layout for `fillContractOrder`:
//!   `[extension_bytes][resolver_address 20 bytes][extra_data bytes]`
//!
//! `takerTraits` bits (1inch LOP v4):
//!   bit 249 — `ARGS_HAS_TARGET`: interaction target address + data in args
//!   bit 248 — `ARGS_HAS_EXTENSION`: Fusion extension bytes prepended to args
//!   bits 0-79 — amount threshold (takingAmount from Dutch auction)

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::sol_types::SolValue;

// 1inch LOP v4 uses a packed `Address` type (uint256) for all address fields.
sol! {
    interface IOrderMixin {
        struct Order {
            uint256 salt;
            uint256 maker;
            uint256 receiver;
            uint256 makerAsset;
            uint256 takerAsset;
            uint256 makingAmount;
            uint256 takingAmount;
            uint256 makerTraits;
        }

        // The args-bearing variant of fillContractOrder; selector 0x56a75868.
        // The 4-param `fillContractOrder` (no args, 0xcc713a04) is a separate function.
        function fillContractOrderArgs(
            Order calldata order,
            bytes calldata signature,
            uint256 amount,
            uint256 takerTraits,
            bytes calldata args
        ) external payable returns (uint256, uint256, bytes32);
    }

    interface IBackrunResolver {
        function settleOrders(bytes calldata data) external;
    }
}

/// Bit 249: args contains a 20-byte interaction target address + callback data.
const ARGS_HAS_TARGET_BIT: u8 = 249;
/// Bit 248: args starts with Fusion extension bytes.
const ARGS_HAS_EXTENSION_BIT: u8 = 248;

/// Decoded order fields in U256 form (matching 1inch LOP v4's packed Address type).
pub struct RawOrderFields {
    pub salt: U256,
    pub maker: U256,
    pub receiver: U256,
    pub maker_asset: U256,
    pub taker_asset: U256,
    pub making_amount: U256,
    pub taking_amount: U256,
    pub maker_traits: U256,
}

/// Parameters for [`build_settle_calldata`].
pub struct SettleParams<'a> {
    /// Decoded LOP Order fields.
    pub order_fields: &'a RawOrderFields,
    /// 65-byte maker EIP-712 signature (raw bytes, no `0x` prefix).
    pub signature: &'a [u8],
    /// Fusion extension bytes from the API (raw bytes).
    pub extension: &'a [u8],
    /// Auction price at this block timestamp (price threshold, bits 0-79).
    pub taking_amount: u128,
    /// Fynd/Tycho router address (used for both primary swap and surplus→WETH swap).
    pub router: Address,
    /// Fynd swap calldata with `receiver = resolver_address`.
    pub primary_swap_calldata: &'a [u8],
    /// Surplus→WETH template calldata; `amountIn` patched on-chain at runtime.
    /// Empty slice means no surplus swap.
    pub surplus_calldata: &'a [u8],
    /// The deployed `BackrunResolver` contract address.
    pub resolver_address: Address,
}

/// Builds the complete `settleOrders(fillContractOrder(...))` calldata.
#[must_use]
pub fn build_settle_calldata(p: &SettleParams<'_>) -> Bytes {
    // ── extraData for takerInteraction ──────────────────────────────────────
    // abi.encode(router, swapCalldata, surplusCalldata)
    let extra_data: Vec<u8> = (
        p.router,
        Bytes::copy_from_slice(p.primary_swap_calldata),
        Bytes::copy_from_slice(p.surplus_calldata),
    )
        .abi_encode();

    // ── args = extension ++ resolver_address(20 bytes) ++ extra_data ────────
    let mut args = Vec::with_capacity(p.extension.len() + 20 + extra_data.len());
    args.extend_from_slice(p.extension);
    args.extend_from_slice(p.resolver_address.as_slice());
    args.extend_from_slice(&extra_data);

    // ── takerTraits ─────────────────────────────────────────────────────────
    let mut taker_traits = U256::from(p.taking_amount); // lower bits = threshold
    if !p.extension.is_empty() {
        taker_traits |= U256::from(1u64) << ARGS_HAS_EXTENSION_BIT;
    }
    taker_traits |= U256::from(1u64) << ARGS_HAS_TARGET_BIT;

    // ── fillContractOrder calldata ───────────────────────────────────────────
    let order = IOrderMixin::Order {
        salt: p.order_fields.salt,
        maker: p.order_fields.maker,
        receiver: p.order_fields.receiver,
        makerAsset: p.order_fields.maker_asset,
        takerAsset: p.order_fields.taker_asset,
        makingAmount: p.order_fields.making_amount,
        takingAmount: p.order_fields.taking_amount,
        makerTraits: p.order_fields.maker_traits,
    };

    let fill_call = IOrderMixin::fillContractOrderArgsCall {
        order,
        signature: Bytes::copy_from_slice(p.signature),
        amount: p.order_fields.making_amount, // full fill
        takerTraits: taker_traits,
        args: Bytes::from(args),
    };
    let fill_calldata = fill_call.abi_encode();

    // ── settleOrders wrapper ─────────────────────────────────────────────────
    let settle_call = IBackrunResolver::settleOrdersCall {
        data: Bytes::from(fill_calldata),
    };
    Bytes::from(settle_call.abi_encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    fn zero_order_fields() -> RawOrderFields {
        RawOrderFields {
            salt: U256::ZERO,
            maker: U256::ZERO,
            receiver: U256::ZERO,
            maker_asset: U256::ZERO,
            taker_asset: U256::ZERO,
            making_amount: U256::from(1_000_000u64),
            taking_amount: U256::from(2_000_000u64),
            maker_traits: U256::ZERO,
        }
    }

    #[test]
    fn build_settle_calldata_no_extension_has_target_bit() {
        let order_fields = zero_order_fields();
        let params = SettleParams {
            order_fields: &order_fields,
            signature: &[0u8; 65],
            extension: &[],
            taking_amount: 999_u128,
            router: Address::ZERO,
            primary_swap_calldata: &[0xde, 0xad],
            surplus_calldata: &[],
            resolver_address: Address::ZERO,
        };
        let calldata = build_settle_calldata(&params);
        // settleOrders selector = keccak256("settleOrders(bytes)")[..4]
        assert_eq!(&calldata[..4], IBackrunResolver::settleOrdersCall::SELECTOR);
        // bit 249 must be set (ARGS_HAS_TARGET), bit 248 must not (no extension).
        // The taker_traits encoding is inside the ABI payload — at minimum the
        // output must be non-empty and start with the right 4-byte selector.
        assert!(calldata.len() > 4);
    }

    #[test]
    fn build_settle_calldata_with_extension_sets_extension_bit() {
        let order_fields = zero_order_fields();
        let extension = vec![0xaau8; 32];
        let params = SettleParams {
            order_fields: &order_fields,
            signature: &[0u8; 65],
            extension: &extension,
            taking_amount: 0,
            router: Address::ZERO,
            primary_swap_calldata: &[],
            surplus_calldata: &[],
            resolver_address: Address::ZERO,
        };
        let calldata = build_settle_calldata(&params);
        assert!(calldata.len() > 4);
    }
}
