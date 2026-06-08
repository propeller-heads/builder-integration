// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { Script, console2 } from "forge-std/Script.sol";
import { BackrunResolver } from "../src/BackrunResolver.sol";

/// @notice Deploys BackrunResolver.
///
/// Token approvals are granted automatically on the first order fill; no
/// manual pre-approval is required.
///
/// Usage:
///   forge script script/Deploy.s.sol \
///     --rpc-url $MAINNET_RPC_URL \
///     --private-key $DEPLOYER_PRIVATE_KEY \
///     --broadcast \
///     --verify \
///     --etherscan-api-key $ETHERSCAN_API_KEY
///
/// Required env vars:
///   DEPLOYER_PRIVATE_KEY  — EOA that receives DEFAULT_ADMIN_ROLE and EXECUTOR_ROLE
///   MAINNET_RPC_URL       — Ethereum JSON-RPC endpoint
/// Optional:
///   ETHERSCAN_API_KEY     — for contract verification
contract Deploy is Script {
    address constant LOP  = 0x111111125421cA6dc452d289314280a0f8842A65;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

    function run() external {
        uint256 deployerKey = vm.envUint("DEPLOYER_PRIVATE_KEY");
        address deployer    = vm.addr(deployerKey);

        vm.startBroadcast(deployerKey);
        BackrunResolver resolver = new BackrunResolver(LOP, WETH);
        vm.stopBroadcast();

        console2.log("BackrunResolver deployed at:", address(resolver));
        console2.log("Admin / first executor:     ", deployer);
        console2.log("Grant additional executors with:");
        console2.log("  resolver.grantRole(EXECUTOR_ROLE, <address>)");
    }
}
