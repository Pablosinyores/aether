// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {AetherExecutor} from "../src/AetherExecutor.sol";

contract DeployAetherExecutor is Script {
    // Mainnet Aave V3 Pool
    address constant DEFAULT_AAVE_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    // Mainnet Balancer V2 Vault
    address constant DEFAULT_BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    // Mainnet Bancor V3 BancorNetwork router
    address constant DEFAULT_BANCOR_NETWORK = 0xeEF417e1D5CC832e619ae18D2F140De2999dD4fB;

    function runWithParams(address aavePool, address balancerVault, address bancorNetwork) public returns (AetherExecutor) {
        vm.startBroadcast();
        AetherExecutor executor = new AetherExecutor(aavePool, balancerVault, bancorNetwork);
        vm.stopBroadcast();

        console.log("AetherExecutor deployed at:", address(executor));
        console.log("Owner:", executor.owner());
        console.log("Aave Pool:", executor.aavePool());
        console.log("Balancer Vault:", executor.protocolRouter(5)); // BALANCER_V2
        console.log("Bancor Network:", executor.protocolRouter(6)); // BANCOR_V3

        return executor;
    }

    function run() external returns (AetherExecutor) {
        address aavePool = vm.envOr("AAVE_POOL", DEFAULT_AAVE_POOL);
        address balancerVault = vm.envOr("BALANCER_VAULT", DEFAULT_BALANCER_VAULT);
        address bancorNetwork = vm.envOr("BANCOR_NETWORK", DEFAULT_BANCOR_NETWORK);
        return runWithParams(aavePool, balancerVault, bancorNetwork);
    }
}
