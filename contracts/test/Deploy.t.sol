// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {DeployAetherExecutor} from "../script/Deploy.s.sol";
import {AetherExecutor} from "../src/AetherExecutor.sol";

contract DeployTest is Test {
    DeployAetherExecutor deployer;

    address constant DEFAULT_AAVE_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    address constant DEFAULT_BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address constant DEFAULT_BANCOR_NETWORK = 0xeEF417e1D5CC832e619ae18D2F140De2999dD4fB;

    function setUp() public {
        deployer = new DeployAetherExecutor();
    }

    function test_deploy_defaultAavePool() public {
        AetherExecutor executor = deployer.runWithParams(DEFAULT_AAVE_POOL, DEFAULT_BALANCER_VAULT, DEFAULT_BANCOR_NETWORK);
        assertEq(executor.aavePool(), DEFAULT_AAVE_POOL);
        assertEq(executor.balancerVault(), DEFAULT_BALANCER_VAULT);
        assertEq(executor.bancorNetwork(), DEFAULT_BANCOR_NETWORK);
        assertEq(executor.owner(), tx.origin);
    }

    function test_deploy_ownerIsDeployer() public {
        AetherExecutor executor = deployer.runWithParams(DEFAULT_AAVE_POOL, DEFAULT_BALANCER_VAULT, DEFAULT_BANCOR_NETWORK);
        assertEq(executor.owner(), tx.origin);
        assertTrue(executor.owner() != address(0));
    }
}

contract DeployCustomPoolTest is Test {
    DeployAetherExecutor deployer;

    address constant CUSTOM_POOL = address(0xBEEF);
    address constant CUSTOM_VAULT = address(0xCAFE);
    address constant CUSTOM_BANCOR = address(0xBAAC);

    function setUp() public {
        deployer = new DeployAetherExecutor();
    }

    function test_deploy_customAavePool() public {
        AetherExecutor executor = deployer.runWithParams(CUSTOM_POOL, CUSTOM_VAULT, CUSTOM_BANCOR);
        assertEq(executor.aavePool(), CUSTOM_POOL);
        assertEq(executor.balancerVault(), CUSTOM_VAULT);
        assertEq(executor.bancorNetwork(), CUSTOM_BANCOR);
    }
}
