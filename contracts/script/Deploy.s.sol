// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {AetherExecutor} from "../src/AetherExecutor.sol";

contract DeployAetherExecutor is Script {
    // Mainnet Aave V3 Pool
    address constant DEFAULT_AAVE_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;

    function runWithAavePool(address aavePool) public returns (AetherExecutor) {
        vm.startBroadcast();
        AetherExecutor executor = new AetherExecutor(aavePool);
        vm.stopBroadcast();

        console.log("AetherExecutor deployed at:", address(executor));
        console.log("Owner:", executor.owner());
        console.log("Aave Pool:", executor.aavePool());

        return executor;
    }

    function run() external returns (AetherExecutor) {
        // Allow override via env var, fall back to mainnet default
        address aavePool = vm.envOr("AAVE_POOL", DEFAULT_AAVE_POOL);
        return runWithAavePool(aavePool);
    }
}
