// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { Script, console2 } from "forge-std/Script.sol";
import { BackrunResolver } from "../src/BackrunResolver.sol";

/// @notice Adds an executor EOA to a deployed BackrunResolver.
///
/// Usage:
///   RESOLVER_ADDRESS=0x... EXECUTOR_ADDRESS=0x... \
///   forge script script/AddExecutor.s.sol \
///     --rpc-url $MAINNET_RPC_URL \
///     --private-key $OWNER_PRIVATE_KEY \
///     --broadcast
contract AddExecutor is Script {
    function run() external {
        address resolverAddr = vm.envAddress("RESOLVER_ADDRESS");
        address executorAddr = vm.envAddress("EXECUTOR_ADDRESS");
        uint256 ownerKey     = vm.envUint("OWNER_PRIVATE_KEY");

        vm.startBroadcast(ownerKey);
        BackrunResolver(payable(resolverAddr)).addExecutor(executorAddr);
        vm.stopBroadcast();

        console2.log("Added executor:", executorAddr);
        console2.log("To resolver:   ", resolverAddr);
    }
}
