// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/AetherExecutor.sol";

contract AetherExecutorTest is Test {
    AetherExecutor executor;

    function setUp() public {
        executor = new AetherExecutor();
    }

    function test_owner() public view {
        assertEq(executor.owner(), address(this));
    }
}
