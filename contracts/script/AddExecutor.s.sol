// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { Script, console2 } from "forge-std/Script.sol";
import { BackrunResolver } from "../src/BackrunResolver.sol";

/// @notice Grants EXECUTOR_ROLE to an EOA on a deployed BackrunResolver.
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

        BackrunResolver resolver = BackrunResolver(payable(resolverAddr));

        vm.startBroadcast(ownerKey);
        resolver.grantRole(resolver.EXECUTOR_ROLE(), executorAddr);
        vm.stopBroadcast();

        console2.log("Granted EXECUTOR_ROLE to:", executorAddr);
        console2.log("On resolver:             ", resolverAddr);
    }
}
