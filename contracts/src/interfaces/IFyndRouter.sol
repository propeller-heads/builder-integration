// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

/// @dev Fynd/Tycho router on Ethereum mainnet: 0x1f8dB310f32D48B6180fF902EC60C586128cEf47
///
/// ABI reference for off-chain calldata construction.
/// The resolver calls this via low-level `call(calldata)` — these function definitions
/// are for documentation and cast/abi decoding only.
interface IFyndRouter {
    struct ClientFeeParams {
        uint16 bps;
        address receiver;
        uint256 maxContribution;
        uint256 deadline;
        bytes signature;
    }

    /// @dev Selector 0xce25e49e. Receiver is param[4] at calldata byte offset 132.
    function singleSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) external payable returns (uint256 amountOut);

    /// @dev Receiver is param[4] at calldata byte offset 132.
    function sequentialSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) external payable returns (uint256 amountOut);

    /// @dev nTokens is param[4]; receiver is param[5] at calldata byte offset 164.
    function splitSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) external payable returns (uint256 amountOut);
}
