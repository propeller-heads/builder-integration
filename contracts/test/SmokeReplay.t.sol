// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { Test } from "forge-std/Test.sol";
import { BackrunResolver } from "src/BackrunResolver.sol";

/// @notice Replays the successful eth_call from smoke run 3, block 25230553.
///
/// INFO smoke: eth_call SUCCESS ✓  block_number=25230553  tx_index=0
///
/// Order: 0x9d7a1175ffd8e3b62e32c68657a1fa7bc08a7d8f07161a31ce9ce14560448c54
///   makerAsset: FABA (0xfaba6f8e4a5e8ab82f62fe7c39859fa577269be3)
///   takerAsset: WETH (0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2)
///   amount_out:     14 597 230 149 099 227 wei WETH (Fynd quote)
///   onchain_taking: 14 581 144 812 870 894 wei WETH (required by auction)
///   surplus:            16 085 336 228 333 wei
///
/// Setup mirrors the smoke binary's state overrides exactly:
///   1. BackrunResolver bytecode etched at VIRTUAL_RESOLVER
///   2. EXECUTOR_ROLE granted to VIRTUAL_RESOLVER in its own AccessControl storage
///   3. Fynd FeeCalculator: client fee set to 0 bps for our resolver
///   4. block.timestamp warped to confirmed+12 s (pending block timestamp)
contract SmokeReplayTest is Test {
    address constant LOP              = 0x111111125421cA6dc452d289314280a0f8842A65;
    address constant WETH             = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

    /// Virtual resolver — bottom 10 bytes match the first whitelisted Fusion resolver entry.
    /// Fusion v2 whitelist check: uint80(uint160(taker)) == entry_low80.
    address constant VIRTUAL_RESOLVER = 0x00000000000000000000b09498030ae3416b66Dc;

    /// Fynd fee calculator — storage-overridden to 0 bps for VIRTUAL_RESOLVER.
    address constant FEE_CALCULATOR   = 0x24AD1d4a2666a99Ef46adA68999a89E324CD914C;

    /// Fork at the confirmed parent of the pending block we were building.
    uint256 constant FORK_BLOCK        = 25_230_552;

    /// Pending block timestamp: block 25230552 timestamp (1780413599) + 12 s.
    uint64  constant PENDING_TIMESTAMP = 1_780_413_611;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);

        // Deploy to get runtime bytecode with correct LOP/WETH immutables, then etch at
        // the virtual address (bypasses constructor — only the code matters here).
        BackrunResolver temp = new BackrunResolver(LOP, WETH);
        vm.etch(VIRTUAL_RESOLVER, address(temp).code);

        // Grant EXECUTOR_ROLE to VIRTUAL_RESOLVER in its own AccessControl storage.
        // OZ AccessControl layout (slot 0: mapping(bytes32 => RoleData)):
        //   _roles[role].members[account] → keccak256(account || keccak256(role || 0))
        bytes32 executorRole = keccak256("EXECUTOR_ROLE");
        bytes32 roleDataSlot = keccak256(abi.encode(executorRole, uint256(0)));
        bytes32 hasRoleSlot  = keccak256(abi.encode(VIRTUAL_RESOLVER, roleDataSlot));
        vm.store(VIRTUAL_RESOLVER, hasRoleSlot, bytes32(uint256(1)));

        // Fynd fee calculator: mapping(address => packed_config) at slot 2.
        // packed_config: bit 0 = isSet; bits 8-23 = fee bps.
        // Value 1 → isSet=true, fee=0 bps.
        bytes32 clientSlot = keccak256(abi.encode(VIRTUAL_RESOLVER, uint256(2)));
        vm.store(FEE_CALCULATOR, clientSlot, bytes32(uint256(1)));

        // Warp to the pending block timestamp so the Fusion Dutch auction sees the
        // correct elapsed time when computing the required taking amount.
        // baseFee comes from the fork (1 871 798 811 wei) — no vm.fee() needed.
        vm.warp(PENDING_TIMESTAMP);
    }

    /// @dev Replays the exact calldata that produced `eth_call SUCCESS ✓` in the smoke log.
    ///      Calldata is stored in test/fixtures/smoke_replay_25230553.hex.
    function test_replaySmokeSuccess_block25230553() public {
        bytes memory data = vm.parseBytes(vm.readFile("test/fixtures/smoke_replay_25230553.hex"));

        vm.prank(VIRTUAL_RESOLVER);
        (bool ok, bytes memory ret) = VIRTUAL_RESOLVER.call(data);
        if (!ok) {
            assembly ("memory-safe") { revert(add(ret, 32), mload(ret)) }
        }
    }
}
