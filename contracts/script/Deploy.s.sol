// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {AetherExecutor} from "../src/AetherExecutor.sol";

contract DeployAetherExecutor is Script {
    // Mainnet Aave V3 Pool
    address constant DEFAULT_AAVE_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    // Mainnet Balancer V2 Vault
    address constant DEFAULT_BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;

    function runWithParams(address aavePool, address balancerVault) public returns (AetherExecutor) {
        vm.startBroadcast();
        AetherExecutor executor = new AetherExecutor(aavePool, balancerVault);
        vm.stopBroadcast();

        console.log("AetherExecutor deployed at:", address(executor));
        console.log("Owner:", executor.owner());
        console.log("Aave Pool:", executor.aavePool());
        console.log("Balancer Vault:", executor.balancerVault());

        return executor;
    }

    function run() external returns (AetherExecutor) {
        address aavePool = vm.envOr("AAVE_POOL", DEFAULT_AAVE_POOL);
        address balancerVault = vm.envOr("BALANCER_VAULT", DEFAULT_BALANCER_VAULT);
        return runWithParams(aavePool, balancerVault);
    }
}
