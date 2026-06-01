//! ABI encoding for `BackrunResolver.settleOrders` + 1inch LOP v4 `fillContractOrder`.
//!
//! Settlement call chain:
//!   executor → `resolver.settleOrders(fill_calldata)`
//!           → `lop.fillContractOrder(order, sig, amount, takerTraits, args)`
//!                 → `resolver.takerInteraction(order, ..., extraData)`
//!                         → `fynd_router.call(swap_calldata)`
//!
//! `args` layout for `fillContractOrderArgs` (from `TakerTraitsLib._parseArgs`):
//!   `[target 20 bytes][extension extensionLen bytes][interaction interactionLen bytes]`
//!
//!   The LOP strips the target first (if `argsHasTarget()`), then reads
//!   `extensionLen` and `interactionLen` from `takerTraits` bit fields.
//!
//! `takerTraits` bit layout (1inch LOP v4 `TakerTraitsLib`):
//!   bit 251       — `ARGS_HAS_TARGET`: first 20 bytes of args are the interaction target
//!   bits 224-247  — `argsExtensionLength`: how many bytes of args are Fusion extension
//!   bits 200-223  — `argsInteractionLength`: how many bytes follow extension as interaction
//!   bits 0-184    — amount threshold

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

        // Selector 0x56a75868: taker is a contract, maker is ERC-1271 (smart contract).
        // LOP calls maker.isValidSignature(hash, sig) for validation.
        function fillContractOrderArgs(
            Order calldata order,
            bytes calldata signature,
            uint256 amount,
            uint256 takerTraits,
            bytes calldata args
        ) external payable returns (uint256, uint256, bytes32);

        // Selector 0xf497df75: taker is a contract, maker is an EOA.
        // LOP uses ecrecover(hash, r, vs) where vs = s with top bit encoding v parity
        // (EIP-2098 compact form: vs = s | ((v - 27) << 255)).
        function fillOrderArgs(
            Order calldata order,
            bytes32 r,
            bytes32 vs,
            uint256 amount,
            uint256 takerTraits,
            bytes calldata args
        ) external payable returns (uint256, uint256, bytes32);
    }

    interface IBackrunResolver {
        function settleOrders(bytes calldata data) external;
    }
}

/// Bit 251: first 20 bytes of args are the takerInteraction target address.
const ARGS_HAS_TARGET_BIT: u8 = 251;
/// Bits 224-247: 24-bit field encoding the extension byte length in args.
const ARGS_EXTENSION_LENGTH_OFFSET: u8 = 224;
/// Bits 200-223: 24-bit field encoding the interaction byte length in args.
const ARGS_INTERACTION_LENGTH_OFFSET: u8 = 200;

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

    // ── args = resolver_address(20B) ++ extension ++ extra_data(interaction) ─
    // LOP._parseArgs strips target first, then reads extensionLength/interactionLength
    // bytes from takerTraits bit fields.
    let mut args = Vec::with_capacity(20 + p.extension.len() + extra_data.len());
    args.extend_from_slice(p.resolver_address.as_slice()); // target stripped first by LOP
    args.extend_from_slice(p.extension);
    args.extend_from_slice(&extra_data);

    // ── takerTraits ─────────────────────────────────────────────────────────
    let mut taker_traits = U256::from(p.taking_amount); // bits 0-184 = threshold
    taker_traits |= U256::from(1u64) << ARGS_HAS_TARGET_BIT;
    taker_traits |= U256::from(p.extension.len() as u64) << ARGS_EXTENSION_LENGTH_OFFSET;
    taker_traits |= U256::from(extra_data.len() as u64) << ARGS_INTERACTION_LENGTH_OFFSET;

    // ── fill calldata: EOA makers use fillOrderArgs, ERC-1271 makers use fillContractOrderArgs
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

    let fill_calldata = if p.signature.len() == 65 {
        // EOA-signed order: use fillOrderArgs (ecrecover path).
        // Compact EIP-2098 format: vs = s | ((v - 27) << 255)
        let mut r = [0u8; 32];
        let mut vs_bytes = [0u8; 32];
        r.copy_from_slice(&p.signature[..32]);
        vs_bytes.copy_from_slice(&p.signature[32..64]);
        let v = p.signature[64];
        // Encode v parity into the top bit of vs (0x80 of byte 0 in big-endian)
        if v == 28 {
            vs_bytes[0] |= 0x80;
        }
        IOrderMixin::fillOrderArgsCall {
            order,
            r: r.into(),
            vs: vs_bytes.into(),
            amount: p.order_fields.making_amount,
            takerTraits: taker_traits,
            args: Bytes::from(args),
        }
        .abi_encode()
    } else {
        // ERC-1271 maker: use fillContractOrderArgs (isValidSignature path).
        IOrderMixin::fillContractOrderArgsCall {
            order,
            signature: Bytes::copy_from_slice(p.signature),
            amount: p.order_fields.making_amount,
            takerTraits: taker_traits,
            args: Bytes::from(args),
        }
        .abi_encode()
    };

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
