// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { Script, console2 } from "forge-std/Script.sol";
import { BackrunResolver } from "../src/BackrunResolver.sol";

/// @notice Deploys BackrunResolver locally (no broadcast) and writes the runtime
///         bytecode to `out/BackrunResolver.runtime.hex`.
///
/// The hex file can be used as a state override in `eth_call` requests, allowing
/// the backrunner smoke test to validate fill calldata without a live deployment.
///
/// Usage:
///   forge script script/PrintBytecode.s.sol --silent
///
/// Output: out/BackrunResolver.runtime.hex
contract PrintBytecode is Script {
    address constant LOP  = 0x111111125421cA6dc452d289314280a0f8842A65;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

    function run() external {
        // Deploy without broadcasting so immutables are baked with the real mainnet addresses.
        BackrunResolver resolver = new BackrunResolver(LOP, WETH);
        bytes memory code = address(resolver).code;
        string memory encoded = vm.toString(code);

        vm.writeFile("out/BackrunResolver.runtime.hex", encoded);

        console2.log("BackrunResolver runtime bytecode (%d bytes)", code.length);
        console2.log("Written to: out/BackrunResolver.runtime.hex");
    }
}
