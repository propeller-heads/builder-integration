// SPDX-License-Identifier: MIT
pragma solidity ^0.8.25;

import { Test } from "forge-std/Test.sol";
import { IERC20 } from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import { IAccessControl } from "@openzeppelin/contracts/access/IAccessControl.sol";
import { BackrunResolver } from "src/BackrunResolver.sol";
import { Address, AddressLib, Order } from "src/interfaces/I1inchLOP.sol";

// ─── Helpers ────────────────────────────────────────────────────────────────

/// @dev Transfers all WETH it holds to msg.sender when called. Used to
///      exercise the WETH-direct-unwrap path without a real Fynd route.
contract MockSwapRouter {
    address immutable weth;

    constructor(address _weth) {
        weth = _weth;
    }

    fallback() external {
        uint256 bal = IERC20(weth).balanceOf(address(this));
        if (bal > 0) IERC20(weth).transfer(msg.sender, bal);
    }
}

// ─── Test contract ───────────────────────────────────────────────────────────

contract BackrunResolverTest is Test {
    using AddressLib for Address;

    // ── Addresses ────────────────────────────────────────────────────────────

    uint256 constant FORK_BLOCK = 25_209_740;

    address constant LOP         = 0x111111125421cA6dc452d289314280a0f8842A65;
    address constant WETH        = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDC        = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    // Original Fynd router — fixtures were generated for this address at fork block 25209740
    address constant FYND_ROUTER = 0x1f8dB310f32D48B6180fF902EC60C586128cEf47;

    // Anvil account 0 → owner, Anvil account 1 → executor.
    address constant OWNER    = 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266;
    address constant EXECUTOR = 0x70997970C51812dc3A010C7d01b50e0d17dc79C8;

    // Receiver address baked into the fixture calldatas (compiled assuming OWNER nonce=0).
    // On a mainnet fork OWNER has a non-zero nonce so resolver lands elsewhere; we
    // use _patchReceiver to update the fixture bytes at test time.
    address constant FIXTURE_RECEIVER = 0x5FbDB2315678afecb367f032d93F642f64180aa3;

    uint256 constant MIN_AMOUNT_RECEIVED = 2_008_608_829; // ~2008 USDC (6 dec)

    // ── State ────────────────────────────────────────────────────────────────

    BackrunResolver resolver;

    // ── Setup ────────────────────────────────────────────────────────────────

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);

        // Deploy from OWNER. Because OWNER has a non-zero nonce on mainnet the resolver
        // won't land at FIXTURE_RECEIVER, so we capture the actual address and use
        // _patchReceiver in fixture-based tests to redirect the Fynd output correctly.
        vm.startPrank(OWNER);
        resolver = new BackrunResolver(LOP, WETH);
        vm.stopPrank();

        // Cache role constant before setting prank — vm.prank is consumed by the first
        // external call, including STATICCALL; calling resolver.EXECUTOR_ROLE() inside
        // the prank context would consume it before grantRole is reached.
        bytes32 executorRole = resolver.EXECUTOR_ROLE();
        vm.prank(OWNER);
        resolver.grantRole(executorRole, EXECUTOR);
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// @dev Read a hex fixture file and return the decoded bytes.
    function _readFixture(string memory path) internal view returns (bytes memory) {
        return vm.parseBytes(vm.readFile(path));
    }

    /// @dev Replace the `receiver` field (param index 4, calldata word at offset 132) in a
    ///      Fynd singleSwap/sequentialSwap calldata blob.
    function _patchReceiver(bytes memory data, address newReceiver) internal pure {
        require(data.length >= 164, "calldata too short for receiver patch");
        assembly {
            mstore(add(add(data, 32), 132), newReceiver)
        }
    }

    /// @dev Build a minimal Order struct with maker and taker assets set.
    function _makeOrder(address makerAsset, address takerAsset) internal pure returns (Order memory o) {
        o.makerAsset = Address.wrap(uint256(uint160(makerAsset)));
        o.takerAsset = Address.wrap(uint256(uint160(takerAsset)));
    }

    /// @dev Encode extraData for takerInteraction.
    ///      router is used for both the primary swap and the surplus→WETH swap.
    function _extraData(
        address router,
        bytes memory swapData,
        bytes memory surplusData
    ) internal pure returns (bytes memory) {
        return abi.encode(router, swapData, surplusData);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 1. Access-control: settleOrders
    // ═══════════════════════════════════════════════════════════════════════

    function test_settleOrders_revertsForNonExecutor() public {
        address stranger = makeAddr("stranger");
        bytes32 executorRole = resolver.EXECUTOR_ROLE(); // cache before prank
        vm.prank(stranger);
        vm.expectRevert(abi.encodeWithSelector(
            IAccessControl.AccessControlUnauthorizedAccount.selector,
            stranger,
            executorRole
        ));
        resolver.settleOrders(hex"");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 2. Access-control: takerInteraction – caller must be LOP
    // ═══════════════════════════════════════════════════════════════════════

    function test_takerInteraction_revertsForNonLOP() public {
        address attacker = makeAddr("attacker");
        Order memory o = _makeOrder(address(0), USDC);

        vm.prank(attacker);
        vm.expectRevert(BackrunResolver.OnlyLOP.selector);
        resolver.takerInteraction(
            o,
            hex"",
            bytes32(0),
            address(resolver), // taker = resolver (passes taker check)
            0,
            0,
            0,
            hex""
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 3. Access-control: takerInteraction – taker must be resolver
    // ═══════════════════════════════════════════════════════════════════════

    function test_takerInteraction_revertsWhenNotTaker() public {
        Order memory o = _makeOrder(address(0), USDC);

        vm.prank(LOP);
        vm.expectRevert(BackrunResolver.NotTaker.selector);
        resolver.takerInteraction(
            o,
            hex"",
            bytes32(0),
            address(0), // wrong taker
            0,
            0,
            0,
            hex""
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 4. Role management: grant / revoke EXECUTOR_ROLE
    // ═══════════════════════════════════════════════════════════════════════

    function test_addRemoveExecutor() public {
        address newExec = makeAddr("newExec");
        bytes32 role = resolver.EXECUTOR_ROLE();

        assertFalse(resolver.hasRole(role, newExec));

        vm.prank(OWNER);
        resolver.grantRole(role, newExec);
        assertTrue(resolver.hasRole(role, newExec));

        vm.prank(OWNER);
        resolver.revokeRole(role, newExec);
        assertFalse(resolver.hasRole(role, newExec));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 5. Access-control: approve – only DEFAULT_ADMIN_ROLE
    // ═══════════════════════════════════════════════════════════════════════

    function test_approve_revertsForNonOwner() public {
        bytes32 adminRole = resolver.DEFAULT_ADMIN_ROLE(); // cache before prank
        vm.prank(EXECUTOR);
        vm.expectRevert(abi.encodeWithSelector(
            IAccessControl.AccessControlUnauthorizedAccount.selector,
            EXECUTOR,
            adminRole
        ));
        resolver.approve(USDC, FYND_ROUTER);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 6. Primary swap: WETH → USDC via real Fynd calldata (auto-approval)
    // ═══════════════════════════════════════════════════════════════════════

    function test_takerInteraction_primarySwap_weth_to_usdc() public {
        // Fund resolver with 1 WETH (the makerAsset sent by the LOP before callback).
        deal(WETH, address(resolver), 1 ether);

        bytes memory swapCalldata = _readFixture("test/fixtures/weth_usdc_calldata.hex");
        _patchReceiver(swapCalldata, address(resolver));

        bytes memory extra = _extraData(FYND_ROUTER, swapCalldata, hex"");

        // makerAsset = WETH so auto-approval fires for WETH → FYND_ROUTER.
        Order memory o = _makeOrder(WETH, USDC);

        uint256 usdcBefore = IERC20(USDC).balanceOf(address(resolver));

        vm.prank(LOP);
        resolver.takerInteraction(
            o,
            hex"",
            bytes32(0),
            address(resolver),
            1 ether,
            MIN_AMOUNT_RECEIVED, // takingAmount
            0,
            extra
        );

        uint256 received = IERC20(USDC).balanceOf(address(resolver)) - usdcBefore;
        assertGe(received, MIN_AMOUNT_RECEIVED, "received USDC below takingAmount");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 7. Surplus USDC → secondary Fynd swap → WETH → ETH → coinbase
    // ═══════════════════════════════════════════════════════════════════════

    function test_takerInteraction_surplus_usdc_to_eth() public {
        address miner = makeAddr("miner");
        vm.coinbase(miner);

        deal(WETH, address(resolver), 1 ether);

        bytes memory primaryCalldata = _readFixture("test/fixtures/weth_usdc_calldata.hex");
        _patchReceiver(primaryCalldata, address(resolver));

        bytes memory surplusCalldata = _readFixture("test/fixtures/usdc_weth_calldata.hex");
        _patchReceiver(surplusCalldata, address(resolver));

        // takingAmount is less than what the primary swap returns, so there IS surplus.
        uint256 takingAmount = MIN_AMOUNT_RECEIVED - 1_000_000; // 1 USDC below output

        bytes memory extra = _extraData(FYND_ROUTER, primaryCalldata, surplusCalldata);

        // makerAsset = WETH: auto-approval fires for WETH → FYND_ROUTER.
        // surplusCalldata non-empty: auto-approval fires for USDC → FYND_ROUTER.
        Order memory o = _makeOrder(WETH, USDC);

        uint256 usdcBefore = IERC20(USDC).balanceOf(address(resolver));
        uint256 ethBefore = miner.balance;

        vm.prank(LOP);
        resolver.takerInteraction(
            o,
            hex"",
            bytes32(0),
            address(resolver),
            1 ether,
            takingAmount,
            0,
            extra
        );

        uint256 usdcReceived = IERC20(USDC).balanceOf(address(resolver)) - usdcBefore;
        assertGe(usdcReceived, takingAmount, "primary swap did not return enough USDC");
        emit log_named_uint("coinbase ETH gained", miner.balance - ethBefore);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 8. Surplus WETH → direct unwrap → ETH → coinbase (MockSwapRouter)
    // ═══════════════════════════════════════════════════════════════════════

    function test_takerInteraction_surplus_weth_direct_unwrap() public {
        address miner = makeAddr("miner");
        vm.coinbase(miner);

        MockSwapRouter mockRouter = new MockSwapRouter(WETH);

        // Fund the mock router with 2 WETH so it can "return" WETH to the resolver.
        deal(WETH, address(mockRouter), 2 ether);

        // takerAsset = WETH, takingAmount = 1 WETH, surplus = 1 WETH → unwrapped to ETH.
        uint256 takingAmount = 1 ether;

        bytes memory swapCalldata = hex""; // mock router ignores calldata
        // makerAsset = address(0): auto-approval for makerAsset is skipped.
        bytes memory extra = _extraData(address(mockRouter), swapCalldata, hex"");

        Order memory o = _makeOrder(address(0), WETH);

        uint256 ethBefore = miner.balance;

        vm.prank(LOP);
        resolver.takerInteraction(
            o,
            hex"",
            bytes32(0),
            address(resolver),
            2 ether,
            takingAmount,
            0,
            extra
        );

        assertEq(IERC20(WETH).balanceOf(address(resolver)), takingAmount, "WETH balance wrong");
        assertGt(miner.balance, ethBefore, "coinbase did not receive ETH");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 9. InsufficientOutput revert
    // ═══════════════════════════════════════════════════════════════════════

    function test_takerInteraction_revertsOnInsufficientOutput() public {
        deal(WETH, address(resolver), 1 ether);

        bytes memory swapCalldata = _readFixture("test/fixtures/weth_usdc_calldata.hex");
        _patchReceiver(swapCalldata, address(resolver));

        bytes memory extra = _extraData(FYND_ROUTER, swapCalldata, hex"");

        Order memory o = _makeOrder(WETH, USDC);

        // Ask for more USDC than any swap can possibly return.
        uint256 impossibleAmount = type(uint256).max;

        vm.prank(LOP);
        // InsufficientOutput carries arguments; match only the 4-byte selector prefix.
        vm.expectPartialRevert(BackrunResolver.InsufficientOutput.selector);
        resolver.takerInteraction(
            o,
            hex"",
            bytes32(0),
            address(resolver),
            1 ether,
            impossibleAmount,
            0,
            extra
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 10. withdrawETH
    // ═══════════════════════════════════════════════════════════════════════

    function test_withdrawETH() public {
        vm.deal(address(resolver), 1 ether);

        address payable recipient = payable(makeAddr("recipient"));
        uint256 before = recipient.balance;

        vm.prank(OWNER);
        resolver.withdrawETH(recipient);

        assertEq(recipient.balance - before, 1 ether, "ETH not withdrawn");
        assertEq(address(resolver).balance, 0, "resolver should be empty");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 11. withdrawToken
    // ═══════════════════════════════════════════════════════════════════════

    function test_withdrawToken() public {
        deal(USDC, address(resolver), 500e6);

        address recipient = makeAddr("recipient");

        vm.prank(OWNER);
        resolver.withdrawToken(USDC, recipient);

        assertEq(IERC20(USDC).balanceOf(recipient), 500e6, "USDC not withdrawn");
        assertEq(IERC20(USDC).balanceOf(address(resolver)), 0, "resolver USDC not empty");
    }
}
