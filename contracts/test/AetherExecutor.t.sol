// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {AetherExecutor} from "../src/AetherExecutor.sol";

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
    address constant AAVE_POOL = address(0xAA);
    address owner;

    function setUp() public {
        owner = address(this);
        executor = new AetherExecutor(AAVE_POOL);
        token = new MockERC20();
    }

    function test_owner() public view {
        assertEq(executor.owner(), owner);
    }

    function test_aavePool() public view {
        assertEq(executor.aavePool(), AAVE_POOL);
    }

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
        vm.expectRevert("Zero address");
        executor.transferOwnership(address(0));
    }

    function test_rescue() public {
        // Mint tokens to executor
        token.mint(address(executor), 1000);
        assertEq(token.balanceOf(address(executor)), 1000);

        // Rescue tokens
        executor.rescue(address(token), 1000);
        assertEq(token.balanceOf(owner), 1000);
        assertEq(token.balanceOf(address(executor)), 0);
    }

    function test_rescue_revert_notOwner() public {
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.rescue(address(token), 100);
    }

    function test_executeArb_revert_notOwner() public {
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.executeArb(steps, address(token), 1000);
    }

    function test_receive_eth() public {
        vm.deal(address(this), 1 ether);
        (bool success,) = address(executor).call{value: 0.5 ether}("");
        assertTrue(success);
        assertEq(address(executor).balance, 0.5 ether);
    }
}
