// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

/// @dev Packed address used by 1inch LOP v4 (lower 160 bits = address).
type Address is uint256;

library AddressLib {
    function get(Address a) internal pure returns (address) {
        return address(uint160(Address.unwrap(a)));
    }
}

/// @dev 1inch Limit Order Protocol v4 Order struct.
struct Order {
    uint256 salt;
    Address maker;
    Address receiver;
    Address makerAsset;
    Address takerAsset;
    uint256 makingAmount;
    uint256 takingAmount;
    uint256 makerTraits;
}

/// @dev Called by the LOP after transferring maker asset to the taker, before pulling taker asset.
interface ITakerInteraction {
    function takerInteraction(
        Order calldata order,
        bytes calldata extension,
        bytes32 orderHash,
        address taker,
        uint256 makingAmount,
        uint256 takingAmount,
        uint256 remainingMakingAmount,
        bytes calldata extraData
    ) external;
}

/// @dev Minimal surface of 1inch LOP v4 needed by the resolver.
interface IOrderMixin {
    function fillContractOrder(
        Order calldata order,
        bytes calldata signature,
        uint256 amount,
        uint256 takerTraits,
        bytes calldata args
    ) external returns (uint256 makingAmount, uint256 takingAmount, bytes32 orderHash);
}
