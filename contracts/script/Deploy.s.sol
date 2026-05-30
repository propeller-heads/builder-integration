// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { Script, console2 } from "forge-std/Script.sol";
import { BackrunResolver } from "../src/BackrunResolver.sol";

/// @notice Deploys BackrunResolver and grants necessary token approvals.
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
///   DEPLOYER_PRIVATE_KEY  — EOA that becomes OWNER and first executor
///   MAINNET_RPC_URL       — Ethereum JSON-RPC endpoint
/// Optional:
///   ETHERSCAN_API_KEY     — for contract verification
contract Deploy is Script {
    address constant LOP         = 0x111111125421cA6dc452d289314280a0f8842A65;
    address constant WETH        = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant FYND_ROUTER = 0x1f8dB310f32D48B6180fF902EC60C586128cEf47;

    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
    address constant DAI  = 0x6B175474E89094C44Da98b954EedeAC495271d0F;
    address constant WBTC = 0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599;

    function run() external {
        uint256 deployerKey = vm.envUint("DEPLOYER_PRIVATE_KEY");
        address deployer    = vm.addr(deployerKey);

        vm.startBroadcast(deployerKey);

        BackrunResolver resolver = new BackrunResolver(LOP, WETH);
        console2.log("BackrunResolver deployed at:", address(resolver));

        // maker assets → Fynd Router (resolver swaps these in)
        resolver.approve(WETH, FYND_ROUTER);
        resolver.approve(USDC, FYND_ROUTER);
        resolver.approve(USDT, FYND_ROUTER);
        resolver.approve(DAI,  FYND_ROUTER);
        resolver.approve(WBTC, FYND_ROUTER);

        // taker assets → LOP (LOP pulls these after takerInteraction returns)
        resolver.approve(WETH, LOP);
        resolver.approve(USDC, LOP);
        resolver.approve(USDT, LOP);
        resolver.approve(DAI,  LOP);
        resolver.approve(WBTC, LOP);

        vm.stopBroadcast();

        console2.log("Owner:", deployer);
        console2.log("Add executors with AddExecutor.s.sol");
    }
}
