// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { IERC20 } from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import { SafeERC20 } from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import { AccessControl } from "@openzeppelin/contracts/access/AccessControl.sol";
import { Address, AddressLib, ITakerInteraction, Order } from "./interfaces/I1inchLOP.sol";

interface IWETH {
    function withdraw(uint256 wad) external;
}

/// @title BackrunResolver
/// @notice Fills 1inch Fusion orders via Fynd and routes surplus to block.coinbase.
/// @dev Executors call `settleOrders` with calldata constructed by the off-chain backrunner.
///      The LOP calls back into `takerInteraction` during the fill.
///      Token approvals are granted lazily on the first fill that needs them.
///
/// @custom:security Only EXECUTOR_ROLE may call `settleOrders`.
///                  `takerInteraction` is callable only by the LOP with this contract as taker.
contract BackrunResolver is ITakerInteraction, AccessControl {
    using SafeERC20 for IERC20;
    using AddressLib for Address;

    // ─── Errors ────────────────────────────────────────────────────────────────

    error OnlyLOP();
    error NotTaker();
    error SwapFailed(bytes reason);
    error InsufficientOutput(uint256 received, uint256 required);

    // ─── Roles ──────────────────────────────────────────────────────────────────

    bytes32 public constant EXECUTOR_ROLE = keccak256("EXECUTOR_ROLE");

    // ─── Immutables ─────────────────────────────────────────────────────────────

    address public immutable LOP;
    address public immutable WETH;

    // ─── Constructor ─────────────────────────────────────────────────────────────

    /// @param lop   1inch Limit Order Protocol v4 address.
    /// @param weth  WETH address used for surplus unwrapping.
    constructor(address lop, address weth) {
        LOP  = lop;
        WETH = weth;
        _grantRole(DEFAULT_ADMIN_ROLE, msg.sender);
        _grantRole(EXECUTOR_ROLE, msg.sender);
    }

    // ─── External: settlement entry point ────────────────────────────────────────

    /// @notice Forwards an ABI-encoded LOP fill call to the LOP contract.
    /// @dev `data` = abi.encodeWithSelector(fillContractOrder.selector, order, sig, amount, traits, args)
    function settleOrders(bytes calldata data) external onlyRole(EXECUTOR_ROLE) {
        (bool ok, bytes memory reason) = LOP.call(data);
        if (!ok) _reRevert(reason);
    }

    // ─── ITakerInteraction ────────────────────────────────────────────────────────

    /// @notice Called by the LOP during order settlement. Executes the Fynd swap
    ///         and routes surplus to block.coinbase.
    function takerInteraction(
        Order calldata order,
        bytes calldata,   // extension (unused)
        bytes32,          // orderHash (unused)
        address taker,
        uint256 makingAmount,
        uint256 takingAmount,
        uint256,          // remainingMakingAmount (unused)
        bytes calldata extraData
    ) external override {
        if (msg.sender != LOP) revert OnlyLOP();
        if (taker != address(this)) revert NotTaker();

        (
            address router,
            bytes memory swapCalldata,
            bytes memory surplusCalldata
        ) = abi.decode(extraData, (address, bytes, bytes));

        address makerAsset = order.makerAsset.get();
        address takerAsset = order.takerAsset.get();

        // Lazily approve assets on first use.
        if (makerAsset != address(0)) {
            _ensureApproval(makerAsset, router, makingAmount);
        }
        _ensureApproval(takerAsset, LOP, takingAmount);
        if (surplusCalldata.length > 0) {
            _ensureApproval(takerAsset, router, 1);
        }

        uint256 balanceBefore = IERC20(takerAsset).balanceOf(address(this));

        // ── Primary Fynd swap ──────────────────────────────────────────────────
        {
            (bool ok, bytes memory reason) = router.call(swapCalldata);
            if (!ok) revert SwapFailed(reason);
        }

        uint256 received = IERC20(takerAsset).balanceOf(address(this)) - balanceBefore;
        if (received < takingAmount) revert InsufficientOutput(received, takingAmount);

        // ── Surplus → ETH → coinbase ───────────────────────────────────────────
        uint256 surplus = received - takingAmount;
        if (surplus > 0) {
            _convertSurplus(takerAsset, surplus, router, surplusCalldata);
        }

        // takingAmount remains in this contract; LOP pulls it via transferFrom.
    }

    // ─── Admin ─────────────────────────────────────────────────────────────────

    /// @notice Grant unlimited allowance from this contract to `spender`.
    function approve(address token, address spender) external onlyRole(DEFAULT_ADMIN_ROLE) {
        IERC20(token).forceApprove(spender, type(uint256).max);
    }

    /// @notice Drain ETH from the contract to `to`.
    function withdrawETH(address payable to) external onlyRole(DEFAULT_ADMIN_ROLE) {
        (bool ok,) = to.call{ value: address(this).balance }("");
        require(ok, "ETH transfer failed");
    }

    /// @notice Drain an ERC-20 token from the contract to `to`.
    function withdrawToken(address token, address to) external onlyRole(DEFAULT_ADMIN_ROLE) {
        IERC20(token).safeTransfer(to, IERC20(token).balanceOf(address(this)));
    }

    receive() external payable {}

    // ─── Internal ─────────────────────────────────────────────────────────────────

    function _ensureApproval(address token, address spender, uint256 minAmount) internal {
        if (IERC20(token).allowance(address(this), spender) < minAmount) {
            IERC20(token).forceApprove(spender, type(uint256).max);
        }
    }

    /// @dev Converts `amount` of `token` to native ETH and pays block.coinbase.
    ///      - If token is WETH: direct withdraw().
    ///      - Otherwise: executes `surplusCalldata` (patching amountIn with `amount`),
    ///        receives WETH at this address, then unwraps.
    ///      Failures in the secondary swap are silently swallowed — surplus is a bonus.
    function _convertSurplus(
        address token,
        uint256 amount,
        address router,
        bytes memory surplusCalldata
    ) internal {
        if (token == WETH) {
            IWETH(WETH).withdraw(amount);
        } else if (router != address(0) && surplusCalldata.length >= 36) {
            // Patch amountIn (bytes 4..36 in calldata) with the actual surplus amount.
            // surplusCalldata is a `bytes memory`; layout in memory:
            //   [0x00..0x1f] : length (32 bytes)
            //   [0x20..0x23] : 4-byte selector
            //   [0x24..0x43] : amountIn (first ABI param, 32 bytes)
            assembly {
                mstore(add(add(surplusCalldata, 0x20), 4), amount)
            }

            uint256 wethBefore = IERC20(WETH).balanceOf(address(this));
            (bool ok,) = router.call(surplusCalldata);
            if (ok) {
                uint256 wethGained = IERC20(WETH).balanceOf(address(this)) - wethBefore;
                if (wethGained > 0) IWETH(WETH).withdraw(wethGained);
            }
            // Silently ignore surplus swap failures.
        }

        uint256 eth = address(this).balance;
        if (eth > 0) {
            // block.coinbase may be address(0) in test environments; the transfer is best-effort.
            (bool sent,) = block.coinbase.call{ value: eth }("");
            sent; // suppress unused-variable warning
        }
    }

    /// @dev Re-reverts with the original revert reason from a failed LOP call.
    function _reRevert(bytes memory reason) internal pure {
        assembly {
            revert(add(reason, 32), mload(reason))
        }
    }
}
