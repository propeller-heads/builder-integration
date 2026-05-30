// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { IERC20 } from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import { SafeERC20 } from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import { Address, AddressLib, ITakerInteraction, Order } from "./interfaces/I1inchLOP.sol";

interface IWETH {
    function withdraw(uint256 wad) external;
}

/// @title BackrunResolver
/// @notice Fills 1inch Fusion orders via Fynd and routes surplus to block.coinbase.
/// @dev Executors call `settleOrders` with calldata constructed by the off-chain backrunner.
///      The LOP calls back into `takerInteraction` during the fill.
///
///      TOKEN APPROVALS required before any fill:
///        owner.approve(makerAsset, FYND_ROUTER)   — resolver sends makerAsset into the swap
///        owner.approve(takerAsset, LOP)            — LOP pulls takerAsset after takerInteraction
///
/// @custom:security Only privileged executors may call `settleOrders`.
///                  `takerInteraction` is callable only by the LOP with this contract as taker.
contract BackrunResolver is ITakerInteraction {
    using SafeERC20 for IERC20;
    using AddressLib for Address;

    // ─── Errors ────────────────────────────────────────────────────────────────

    error OnlyOwner();
    error OnlyLOP();
    error NotTaker();
    error NotExecutor();
    error SwapFailed(bytes reason);
    error InsufficientOutput(uint256 received, uint256 required);

    // ─── Immutables ─────────────────────────────────────────────────────────────

    address public immutable LOP;
    address public immutable WETH;
    address public immutable OWNER;

    // ─── Storage ─────────────────────────────────────────────────────────────────

    mapping(address => bool) public executors;

    // ─── Modifiers ───────────────────────────────────────────────────────────────

    modifier onlyOwner() {
        if (msg.sender != OWNER) revert OnlyOwner();
        _;
    }

    modifier onlyExecutor() {
        if (!executors[msg.sender]) revert NotExecutor();
        _;
    }

    // ─── Constructor ─────────────────────────────────────────────────────────────

    /// @param lop   1inch Limit Order Protocol v4 address.
    /// @param weth  WETH address used for surplus unwrapping.
    constructor(address lop, address weth) {
        LOP  = lop;
        WETH = weth;
        OWNER = msg.sender;
        executors[msg.sender] = true;
    }

    // ─── External: settlement entry point ────────────────────────────────────────

    /// @notice Forwards an ABI-encoded LOP fill call to the LOP contract.
    /// @dev The off-chain backrunner constructs `data` as:
    ///      abi.encodeWithSelector(IOrderMixin.fillContractOrder.selector, order, sig, amount, traits, args)
    ///      where `args` contains the ABI-encoded takerInteraction data (extraData).
    function settleOrders(bytes calldata data) external onlyExecutor {
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
        uint256,          // makingAmount (baked into swapCalldata by the backrunner)
        uint256 takingAmount,
        uint256,          // remainingMakingAmount (unused)
        bytes calldata extraData
    ) external override {
        if (msg.sender != LOP) revert OnlyLOP();
        if (taker != address(this)) revert NotTaker();

        (
            address fyndRouter,
            bytes memory swapCalldata,
            address surplusRouter,
            bytes memory surplusCalldata
        ) = abi.decode(extraData, (address, bytes, address, bytes));

        address takerAsset = order.takerAsset.get();

        uint256 balanceBefore = IERC20(takerAsset).balanceOf(address(this));

        // ── Primary Fynd swap ──────────────────────────────────────────────────
        {
            (bool ok, bytes memory reason) = fyndRouter.call(swapCalldata);
            if (!ok) revert SwapFailed(reason);
        }

        uint256 received = IERC20(takerAsset).balanceOf(address(this)) - balanceBefore;
        if (received < takingAmount) revert InsufficientOutput(received, takingAmount);

        // ── Surplus → ETH → coinbase ───────────────────────────────────────────
        uint256 surplus = received - takingAmount;
        if (surplus > 0) {
            _convertSurplus(takerAsset, surplus, surplusRouter, surplusCalldata);
        }

        // takingAmount remains in this contract; LOP pulls it via transferFrom.
    }

    // ─── Owner management ─────────────────────────────────────────────────────────

    /// @notice Grant unlimited allowance from this contract to `spender`.
    ///         Call once per (token, spender) pair: approve(makerAsset, FYND_ROUTER)
    ///         and approve(takerAsset, LOP) before the first fill.
    function approve(address token, address spender) external onlyOwner {
        IERC20(token).forceApprove(spender, type(uint256).max);
    }

    /// @notice Add `executor` to the privileged executor set.
    function addExecutor(address executor) external onlyOwner {
        executors[executor] = true;
    }

    /// @notice Remove `executor` from the privileged executor set.
    function removeExecutor(address executor) external onlyOwner {
        executors[executor] = false;
    }

    /// @notice Drain ETH from the contract to `to`.
    function withdrawETH(address payable to) external onlyOwner {
        (bool ok,) = to.call{ value: address(this).balance }("");
        require(ok, "ETH transfer failed");
    }

    /// @notice Drain an ERC-20 token from the contract to `to`.
    function withdrawToken(address token, address to) external onlyOwner {
        IERC20(token).safeTransfer(to, IERC20(token).balanceOf(address(this)));
    }

    receive() external payable {}

    // ─── Internal ─────────────────────────────────────────────────────────────────

    /// @dev Converts `amount` of `token` to native ETH and pays block.coinbase.
    ///      - If token is WETH: direct withdraw().
    ///      - Otherwise: executes `surplusCalldata` (patching amountIn with `amount`),
    ///        receives WETH at this address, then unwraps.
    ///      Failures in the secondary swap are silently swallowed — surplus is a bonus.
    function _convertSurplus(
        address token,
        uint256 amount,
        address surplusRouter,
        bytes memory surplusCalldata
    ) internal {
        if (token == WETH) {
            IWETH(WETH).withdraw(amount);
        } else if (surplusRouter != address(0) && surplusCalldata.length >= 36) {
            // Patch amountIn (bytes 4..36 in calldata) with the actual surplus amount.
            // surplusCalldata is a `bytes memory`; layout in memory:
            //   [0x00..0x1f] : length (32 bytes)
            //   [0x20..0x23] : 4-byte selector
            //   [0x24..0x43] : amountIn (first ABI param, 32 bytes)
            assembly {
                mstore(add(add(surplusCalldata, 0x20), 4), amount)
            }

            uint256 wethBefore = IERC20(WETH).balanceOf(address(this));
            (bool ok,) = surplusRouter.call(surplusCalldata);
            if (ok) {
                uint256 wethGained = IERC20(WETH).balanceOf(address(this)) - wethBefore;
                if (wethGained > 0) IWETH(WETH).withdraw(wethGained);
            }
            // Silently ignore surplus swap failures.
        }

        uint256 eth = address(this).balance;
        if (eth > 0) {
            // block.coinbase may be address(0) on some test environments.
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
