// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {AetherExecutor} from "../src/AetherExecutor.sol";

/// @dev Mock Aave pool that reverts on flashLoanSimple (used to test FlashLoanFailed)
contract RevertingAavePool {
    fallback() external {
        revert("pool reverted");
    }
}

/// @dev Mock ERC20 for testing
contract MockERC20 {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        require(balanceOf[msg.sender] >= amount, "Insufficient balance");
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        require(balanceOf[from] >= amount, "Insufficient balance");
        require(allowance[from][msg.sender] >= amount, "Insufficient allowance");
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        allowance[from][msg.sender] -= amount;
        return true;
    }
}

contract AetherExecutorTest is Test {
    AetherExecutor executor;
    MockERC20 token;
    MockERC20 token2;
    address constant AAVE_POOL = address(0xAA);
    address owner;

    function setUp() public {
        owner = address(this);
        executor = new AetherExecutor(AAVE_POOL);
        token = new MockERC20();
        token2 = new MockERC20();
    }

    // -------------------------------------------------------------------------
    // Basic state
    // -------------------------------------------------------------------------

    function test_owner() public view {
        assertEq(executor.owner(), owner);
    }

    function test_aavePool() public view {
        assertEq(executor.aavePool(), AAVE_POOL);
    }

    // -------------------------------------------------------------------------
    // transferOwnership
    // -------------------------------------------------------------------------

    function test_transferOwnership() public {
        address newOwner = address(0x123);
        executor.transferOwnership(newOwner);
        assertEq(executor.owner(), newOwner);
    }

    function test_transferOwnership_revert_notOwner() public {
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.transferOwnership(address(0x789));
    }

    function test_transferOwnership_revert_zeroAddress() public {
        vm.expectRevert(AetherExecutor.ZeroAddress.selector);
        executor.transferOwnership(address(0));
    }

    // -------------------------------------------------------------------------
    // rescue
    // -------------------------------------------------------------------------

    function test_rescue() public {
        token.mint(address(executor), 1000);
        assertEq(token.balanceOf(address(executor)), 1000);

        executor.rescue(address(token), 1000);
        assertEq(token.balanceOf(owner), 1000);
        assertEq(token.balanceOf(address(executor)), 0);
    }

    function test_rescue_revert_notOwner() public {
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.rescue(address(token), 100);
    }

    // -------------------------------------------------------------------------
    // executeArb - access control
    // -------------------------------------------------------------------------

    function test_executeArb_revert_notOwner() public {
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.executeArb(steps, address(token), 1000);
    }

    // -------------------------------------------------------------------------
    // executeArb - FlashLoanFailed when pool call reverts
    // -------------------------------------------------------------------------

    function test_executeArb_revert_flashLoanFailed() public {
        // Deploy an executor backed by a pool that always reverts
        RevertingAavePool badPool = new RevertingAavePool();
        AetherExecutor executorWithBadPool = new AetherExecutor(address(badPool));

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.expectRevert(AetherExecutor.FlashLoanFailed.selector);
        executorWithBadPool.executeArb(steps, address(token), 1000);
    }

    // -------------------------------------------------------------------------
    // executeOperation - access control
    // -------------------------------------------------------------------------

    function test_executeOperation_revert_notAavePool() public {
        // Calling from an address that is not the Aave pool must revert
        vm.prank(address(0xBB));
        vm.expectRevert(AetherExecutor.NotAavePool.selector);
        executor.executeOperation(address(token), 1000, 5, address(executor), "");
    }

    function test_executeOperation_revert_invalidInitiator() public {
        // Called from the correct Aave pool address but with a foreign initiator
        vm.prank(AAVE_POOL);
        vm.expectRevert(AetherExecutor.InvalidInitiator.selector);
        executor.executeOperation(address(token), 1000, 5, address(0xDEAD), "");
    }

    // -------------------------------------------------------------------------
    // setApprovals
    // -------------------------------------------------------------------------

    function test_setApprovals() public {
        address[] memory tokens = new address[](1);
        address[] memory spenders = new address[](1);
        tokens[0] = address(token);
        spenders[0] = AAVE_POOL;

        executor.setApprovals(tokens, spenders);

        assertEq(token.allowance(address(executor), AAVE_POOL), type(uint256).max);
    }

    function test_setApprovals_multiple() public {
        address[] memory tokens = new address[](2);
        address[] memory spenders = new address[](2);
        tokens[0] = address(token);
        tokens[1] = address(token2);
        spenders[0] = AAVE_POOL;
        spenders[1] = address(0xBB);

        executor.setApprovals(tokens, spenders);

        assertEq(token.allowance(address(executor), AAVE_POOL), type(uint256).max);
        assertEq(token2.allowance(address(executor), address(0xBB)), type(uint256).max);
    }

    function test_setApprovals_revert_notOwner() public {
        address[] memory tokens = new address[](1);
        address[] memory spenders = new address[](1);
        tokens[0] = address(token);
        spenders[0] = AAVE_POOL;

        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.setApprovals(tokens, spenders);
    }

    // -------------------------------------------------------------------------
    // ETH receive
    // -------------------------------------------------------------------------

    function test_receive_eth() public {
        vm.deal(address(this), 1 ether);
        (bool success,) = address(executor).call{value: 0.5 ether}("");
        assertTrue(success);
        assertEq(address(executor).balance, 0.5 ether);
    }

    // -------------------------------------------------------------------------
    // setApprovals - input validation
    // -------------------------------------------------------------------------

    function test_setApprovals_revert_arrayLengthMismatch() public {
        address[] memory tokens = new address[](2);
        address[] memory spenders = new address[](1);
        tokens[0] = address(token);
        tokens[1] = address(token2);
        spenders[0] = AAVE_POOL;

        vm.expectRevert(AetherExecutor.ArrayLengthMismatch.selector);
        executor.setApprovals(tokens, spenders);
    }

    function test_setApprovals_revert_zeroAddressSpender() public {
        address[] memory tokens = new address[](1);
        address[] memory spenders = new address[](1);
        tokens[0] = address(token);
        spenders[0] = address(0);

        vm.expectRevert(AetherExecutor.ZeroAddress.selector);
        executor.setApprovals(tokens, spenders);
    }

    // -------------------------------------------------------------------------
    // Fuzz: setApprovals empty array is a no-op (no revert)
    // -------------------------------------------------------------------------

    function testFuzz_setApprovals_emptyArrays() public {
        address[] memory tokens = new address[](0);
        address[] memory spenders = new address[](0);
        // Should not revert
        executor.setApprovals(tokens, spenders);
    }
}
