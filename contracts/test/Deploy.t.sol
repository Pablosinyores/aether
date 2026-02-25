// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {DeployAetherExecutor} from "../script/Deploy.s.sol";
import {AetherExecutor} from "../src/AetherExecutor.sol";

contract DeployTest is Test {
    DeployAetherExecutor deployer;

    function setUp() public {
        deployer = new DeployAetherExecutor();
    }

    function test_deploy_defaultAavePool() public {
        AetherExecutor executor = deployer.run();
        assertEq(executor.aavePool(), 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2);
        // vm.startBroadcast() without args defaults to tx.origin
        assertEq(executor.owner(), tx.origin);
    }

    function test_deploy_ownerIsDeployer() public {
        AetherExecutor executor = deployer.run();
        // vm.startBroadcast() without args uses tx.origin as the broadcast sender
        assertEq(executor.owner(), tx.origin);
        assertTrue(executor.owner() != address(0));
    }
}

/// @dev Separate contract to isolate vm.setEnv side effects from other tests
contract DeployCustomPoolTest is Test {
    DeployAetherExecutor deployer;

    function setUp() public {
        deployer = new DeployAetherExecutor();
    }

    function test_deploy_customAavePool() public {
        address customPool = address(0xBEEF);
        vm.setEnv("AAVE_POOL", vm.toString(customPool));
        AetherExecutor executor = deployer.run();
        assertEq(executor.aavePool(), customPool);
    }
}
