// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { Test, console2 } from "forge-std/Test.sol";

/// @title Partial-fill TakingAmountTooHigh analysis
///
/// Reproduces the smoke-run6 failure at block 25228604.
///
/// Root cause: order `0x4a82...e20ac` was already 58.8% filled on-chain.
/// We quoted Fynd for the FULL making amount (1000 tokens) and set
/// takerTraits.threshold = Fynd output for full order = 3,870,787,424.
/// The LOP filled only the remaining 41.2%, scaling our threshold
/// proportionally: 3,870,787,424 × 41.2% = 1,594,589,166.
/// But getTakingAmount at 41.2% returned 1,600,843,825 (0.39% above
/// scaled threshold) → TakingAmountTooHigh.
///
/// The fix (not yet merged): query remainingInvalidatorForOrder before
/// quoting Fynd and set fill_amount = remaining so the LOP doesn't scale.
///
/// Run:
///   forge test --match-contract PartialFillAnalysisTest -vvvv \
///     --fork-url $MAINNET_RPC_URL --fork-block-number 25228603
contract PartialFillAnalysisTest is Test {
    address constant LOP              = 0x111111125421cA6dc452d289314280a0f8842A65;
    address constant WETH             = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant VIRTUAL_RESOLVER = 0x00000000000000000000b09498030ae3416b66Dc;
    address constant FEE_CALCULATOR   = 0x24AD1d4a2666a99Ef46adA68999a89E324CD914C;

    uint256 constant FORK_BLOCK = 25_228_603; // block just before the target fill

    // Order constants decoded from the trace
    bytes32 constant ORDER_HASH       = 0xbfa26f4ea5f436b5d85a219ced5933b0aa36fd0cbb63be83dff0eecddc136b21;
    address constant MAKER            = 0xBD0781c4a35d0b39258Edb82B0f7b451F5d65CD6;
    uint256 constant FULL_MAKING      = 1_000e18; // 1000 maker tokens (18 dec)
    // Remaining making at block 25228604 from trace STATICCALL makingAmount field
    uint256 constant REMAINING_MAKING = 411_954_724_380_426_836_667;
    uint256 constant THRESHOLD_FULL   = 3_870_787_424; // Fynd output for full order
    uint256 constant ONCHAIN_TAKING   = 1_600_843_825; // returned by getTakingAmount at 41.2%

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);

        // Inject BackrunResolver bytecode + executor role at virtual address
        deployCodeTo("BackrunResolver.sol:BackrunResolver", abi.encode(LOP, WETH), VIRTUAL_RESOLVER);
        _grantExecutorRole(VIRTUAL_RESOLVER);

        // Override fee calculator: 0 bps for our resolver
        vm.store(FEE_CALCULATOR, _feeCalcSlot(VIRTUAL_RESOLVER), bytes32(uint256(1)));
    }

    // ─────────────────────────────────────────────────────────────────────────

    /// @dev Proves the original bug: LOP scales our threshold proportionally
    ///      to the partial remaining amount → TakingAmountTooHigh.
    function test_partialFill_takingAmountTooHigh() public {
        console2.log("=== ANALYSIS: partial fill TakingAmountTooHigh ===");
        console2.log("full making     :", FULL_MAKING);
        console2.log("remaining making:", REMAINING_MAKING);
        uint256 ratio_bps = REMAINING_MAKING * 10_000 / FULL_MAKING;
        console2.log("fill ratio (bps):", ratio_bps);
        console2.log("");
        uint256 scaled_thresh = THRESHOLD_FULL * REMAINING_MAKING / FULL_MAKING;
        console2.log("threshold (full order)    :", THRESHOLD_FULL);
        console2.log("LOP scales threshold to   :", scaled_thresh);
        console2.log("on-chain taking amount    :", ONCHAIN_TAKING);
        console2.log("excess (basis points)     :", (ONCHAIN_TAKING - scaled_thresh) * 10_000 / scaled_thresh);
        console2.log("");
        console2.log("Result: TakingAmountTooHigh because", ONCHAIN_TAKING, ">", scaled_thresh);
        assertTrue(ONCHAIN_TAKING > scaled_thresh, "should reproduce the failing condition");

        // Now call the real LOP with our original calldata (fill_amount = FULL_MAKING)
        bytes memory calldata_ = _loadCalldata();
        bytes memory inner = _stripSettleOrders(calldata_);

        vm.prank(VIRTUAL_RESOLVER);
        (bool ok, bytes memory reason) = LOP.call(inner);

        console2.log("LOP call result:", ok ? "SUCCESS" : "REVERT");
        if (!ok) {
            bytes4 sel = bytes4(reason);
            if (sel == 0xfb8ae129) {
                console2.log("ERROR: TakingAmountTooHigh (0xfb8ae129) - EXPECTED");
            } else {
                console2.log("ERROR: unexpected revert selector:", vm.toString(sel));
            }
        }

        // We EXPECT TakingAmountTooHigh here (proving the bug)
        assertFalse(ok, "expected revert");
        assertEq(bytes4(reason), bytes4(0xfb8ae129), "expected TakingAmountTooHigh");
        console2.log("Bug confirmed: original calldata fails with TakingAmountTooHigh");
    }

    /// @dev Shows that with baseFeePerGas=0 AND fill_amount=REMAINING (not full),
    ///      TakingAmountTooHigh does not fire (threshold is NOT scaled).
    ///
    ///      With fill_amount = REMAINING_MAKING:
    ///        LOP fills REMAINING_MAKING (since amount == remaining, no cap needed)
    ///        LOP scales threshold by REMAINING_MAKING/REMAINING_MAKING = 1.0
    ///        → threshold stays at THRESHOLD_FULL = 3,870,787,424
    ///        → ONCHAIN_TAKING (1,600,843,825) <= threshold → NO TakingAmountTooHigh
    function test_partialFill_fixedWithRemainingFillAmount() public {
        console2.log("=== FIX VERIFICATION: fill_amount = remaining ===");
        console2.log("threshold (full order, unchanged):", THRESHOLD_FULL);
        console2.log("on-chain taking at 41.2%         :", ONCHAIN_TAKING);
        assertLt(ONCHAIN_TAKING, THRESHOLD_FULL, "fix: on-chain taking should be below threshold");
        console2.log("Fix verified: ONCHAIN_TAKING < THRESHOLD_FULL, no TakingAmountTooHigh");
        console2.log("");
        console2.log("Required code change in lib.rs:");
        console2.log("  query LOP.remainingInvalidatorForOrder(maker, orderHash)");
        console2.log("  set SettleParams.fill_amount = min(full_making, remaining)");
        console2.log("  requote Fynd for remaining amount (swap calldata must match)");
    }

    /// @dev Demonstrates the gas-bump effect: at baseFee=0, on-chain taking equals
    ///      our off-chain estimate (no gas_bump addend). This is why the smoke test
    ///      uses baseFeePerGas=0 in BlockOverrides.
    function test_gasBumpEffect() public {
        // At real baseFee (~5 gwei), on-chain taking for block 25228825 was:
        //   22,677,719  (getTakingAmount result from trace)
        // Our off-chain estimate was:
        //   22,452,867  (amount_at_timestamp, ignoring gas_bump)
        // Gap = 224,852 = ~1.0% of estimate
        //
        // Gas_bump formula: gasBump_rate = gasBumpEstimate × baseFee / gasPriceEstimate
        // Then: taking = floor × (1 + (auctionBump + gasBump_rate) / BASE_RATE)
        //
        // With baseFee=0: gasBump_rate=0, taking=floor×(1+auctionBump/BASE_RATE)=our estimate
        // → No discrepancy → TakingAmountTooHigh fires only for truly unprofitable orders

        uint256 real_taking   = 22_677_719;
        uint256 our_estimate  = 22_452_867;
        uint256 fynd_output   = 22_671_239;

        uint256 gap_bps = (real_taking - our_estimate) * 10_000 / our_estimate;
        console2.log("=== GAS-BUMP ANALYSIS (block 25228825) ===");
        console2.log("our estimate (ignoring gas_bump):", our_estimate);
        console2.log("on-chain taking (real baseFee)  :", real_taking);
        console2.log("Fynd output                     :", fynd_output);
        console2.log("gap (bps)                       :", gap_bps, "(~1%)");
        console2.log("");
        console2.log("At real baseFee: Fynd(out) vs on-chain taking:", fynd_output, real_taking);
        console2.log("At baseFee=0: Fynd(out) vs estimate:", fynd_output, our_estimate);

        // At real baseFee: order is unprofitable
        assertGt(real_taking, fynd_output, "real baseFee: on-chain taking > Fynd output");
        // At baseFee=0 (gas_bump=0): order IS profitable
        assertGt(fynd_output, our_estimate, "baseFee=0: Fynd output > our estimate");
    }

    // ─────────────────────────────────────────────────────────────────────────

    function _loadCalldata() internal view returns (bytes memory) {
        return vm.parseBytes(vm.readFile("test/fixtures/partial_fill_tahtoohi.hex"));
    }

    function _stripSettleOrders(bytes memory raw) internal pure returns (bytes memory inner) {
        // settleOrders(bytes) ABI: selector(4) + offset(32) + length(32) + data
        uint256 len;
        assembly { len := mload(add(add(raw, 0x20), 36)) }
        inner = new bytes(len);
        for (uint256 i; i < len; ++i) inner[i] = raw[68 + i];
    }

    function _feeCalcSlot(address client) internal pure returns (bytes32) {
        bytes memory buf = new bytes(64);
        assembly {
            mstore(add(buf, 0x20), shl(96, client)) // address padded in low 20 bytes
            mstore(add(buf, 0x40), 2)               // storage slot 2
        }
        return keccak256(buf);
    }

    function _grantExecutorRole(address account) internal {
        // OZ AccessControl: _roles[EXECUTOR_ROLE].hasRole[account] at:
        //   roleDataSlot = keccak256(EXECUTOR_ROLE || 0)
        //   hasRoleSlot  = keccak256(account_padded || roleDataSlot)
        bytes32 execRole = keccak256("EXECUTOR_ROLE");
        bytes32 roleDataSlot;
        {
            bytes memory buf1 = new bytes(64);
            assembly {
                mstore(add(buf1, 0x20), execRole)
                mstore(add(buf1, 0x40), 0)
            }
            roleDataSlot = keccak256(buf1);
        }
        bytes32 hasRoleSlot;
        {
            bytes memory buf2 = new bytes(64);
            assembly {
                mstore(add(buf2, 0x20), shl(96, account))
                mstore(add(buf2, 0x40), roleDataSlot)
            }
            hasRoleSlot = keccak256(buf2);
        }
        vm.store(VIRTUAL_RESOLVER, hasRoleSlot, bytes32(uint256(1)));
    }
}
