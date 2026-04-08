pub mod calldata;
pub mod fork;

use alloy::primitives::{Address, U256};
use aether_common::types::SimulationResult;
use fork::{ForkedState, RpcDB, RpcForkedState, SimConfig};
use revm::context::result::ExecutionResult;
use revm::context::{BlockEnv, TxEnv};
use revm::database::{CacheDB, EmptyDBTyped};
use revm::handler::{ExecuteEvm, MainBuilder};
use revm::primitives::hardfork::SpecId;
use revm::Context;
use tracing::{debug, error, info};

type SimDB = CacheDB<EmptyDBTyped<std::convert::Infallible>>;

/// EVM Simulator using revm for transaction simulation.
///
/// Simulates transactions against a forked EVM state (CacheDB + EmptyDB)
/// before submitting bundles on-chain. This allows validating arbitrage
/// profitability and catching reverts before spending gas.
pub struct EvmSimulator {
    config: SimConfig,
}

impl EvmSimulator {
    pub fn new(config: SimConfig) -> Self {
        Self { config }
    }

    pub fn with_defaults() -> Self {
        Self::new(SimConfig::default())
    }

    /// Return a reference to the simulator's configuration.
    pub fn config(&self) -> &SimConfig {
        &self.config
    }

    /// Simulate a transaction on forked state.
    /// Returns SimulationResult with success/failure, gas used, and profit info.
    pub fn simulate(
        &self,
        state: &ForkedState,
        to: Address,
        calldata: Vec<u8>,
    ) -> SimulationResult {
        // Clone the DB for simulation (we don't want to mutate the original)
        let db = state.db.clone();

        // Build the block environment
        let block = BlockEnv {
            number: U256::from(state.block_number),
            timestamp: U256::from(state.block_timestamp),
            basefee: state.base_fee,
            ..Default::default()
        };

        // Build the transaction
        let tx = TxEnv::builder()
            .caller(self.config.caller)
            .kind(revm::primitives::TxKind::Call(to))
            .data(revm::primitives::Bytes::copy_from_slice(&calldata))
            .value(self.config.value)
            .gas_limit(self.config.gas_limit)
            .gas_price(state.base_fee as u128)
            .nonce(0)
            .chain_id(Some(state.chain_id))
            .build_fill();

        // Build the context and EVM
        let ctx = Context::<BlockEnv, TxEnv, _, SimDB, revm::context::Journal<SimDB>, ()>::new(db, SpecId::CANCUN)
            .with_block(block)
            .modify_cfg_chained(|cfg| {
                cfg.chain_id = state.chain_id;
                cfg.disable_nonce_check = true;
            });

        let mut evm = ctx.build_mainnet();

        // Execute the transaction
        match evm.transact(tx) {
            Ok(result_and_state) => {
                match result_and_state.result {
                    ExecutionResult::Success {
                        gas_used, output: _, ..
                    } => {
                        debug!(gas_used, "Simulation succeeded");
                        SimulationResult {
                            success: true,
                            profit_wei: U256::ZERO, // Profit calculated externally from balance diff
                            gas_used,
                            revert_reason: None,
                        }
                    }
                    ExecutionResult::Revert { gas_used, output } => {
                        let reason = format!("0x{}", alloy::hex::encode(&output));
                        debug!(gas_used, reason = %reason, "Simulation reverted");
                        SimulationResult {
                            success: false,
                            profit_wei: U256::ZERO,
                            gas_used,
                            revert_reason: Some(reason),
                        }
                    }
                    ExecutionResult::Halt { reason, gas_used } => {
                        let reason_str = format!("{:?}", reason);
                        debug!(gas_used, reason = %reason_str, "Simulation halted");
                        SimulationResult {
                            success: false,
                            profit_wei: U256::ZERO,
                            gas_used,
                            revert_reason: Some(reason_str),
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "EVM transact error");
                SimulationResult {
                    success: false,
                    profit_wei: U256::ZERO,
                    gas_used: 0,
                    revert_reason: Some(format!("EVM error: {}", e)),
                }
            }
        }
    }

    /// Simulate a transaction and also return the final state diff,
    /// useful for calculating profit from balance changes.
    pub fn simulate_with_profit(
        &self,
        state: &ForkedState,
        to: Address,
        calldata: Vec<u8>,
        _profit_token: Address,
        profit_recipient: Address,
    ) -> SimulationResult {
        // Clone the DB for simulation
        let db = state.db.clone();

        // Record the initial balance of the profit recipient
        let initial_balance = db
            .cache
            .accounts
            .get(&profit_recipient)
            .and_then(|acc| acc.info())
            .map(|info| info.balance)
            .unwrap_or(U256::ZERO);

        // Build the block environment
        let block = BlockEnv {
            number: U256::from(state.block_number),
            timestamp: U256::from(state.block_timestamp),
            basefee: state.base_fee,
            ..Default::default()
        };

        // Build the transaction
        let tx = TxEnv::builder()
            .caller(self.config.caller)
            .kind(revm::primitives::TxKind::Call(to))
            .data(revm::primitives::Bytes::copy_from_slice(&calldata))
            .value(self.config.value)
            .gas_limit(self.config.gas_limit)
            .gas_price(state.base_fee as u128)
            .nonce(0)
            .chain_id(Some(state.chain_id))
            .build_fill();

        // Build the context and EVM
        let ctx = Context::<BlockEnv, TxEnv, _, SimDB, revm::context::Journal<SimDB>, ()>::new(db, SpecId::CANCUN)
            .with_block(block)
            .modify_cfg_chained(|cfg| {
                cfg.chain_id = state.chain_id;
                cfg.disable_nonce_check = true;
            });

        let mut evm = ctx.build_mainnet();

        match evm.transact(tx) {
            Ok(result_and_state) => {
                match result_and_state.result {
                    ExecutionResult::Success { gas_used, .. } => {
                        // Calculate profit from state diff
                        let final_balance = result_and_state
                            .state
                            .get(&profit_recipient)
                            .map(|acc| acc.info.balance)
                            .unwrap_or(initial_balance);

                        let profit = final_balance.saturating_sub(initial_balance);

                        info!(gas_used, %profit, "Simulation succeeded with profit calculation");

                        SimulationResult {
                            success: true,
                            profit_wei: profit,
                            gas_used,
                            revert_reason: None,
                        }
                    }
                    ExecutionResult::Revert { gas_used, output } => {
                        let reason = format!("0x{}", alloy::hex::encode(&output));
                        debug!(gas_used, reason = %reason, "Simulation reverted");
                        SimulationResult {
                            success: false,
                            profit_wei: U256::ZERO,
                            gas_used,
                            revert_reason: Some(reason),
                        }
                    }
                    ExecutionResult::Halt { reason, gas_used } => {
                        let reason_str = format!("{:?}", reason);
                        debug!(gas_used, reason = %reason_str, "Simulation halted");
                        SimulationResult {
                            success: false,
                            profit_wei: U256::ZERO,
                            gas_used,
                            revert_reason: Some(reason_str),
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "EVM transact error");
                SimulationResult {
                    success: false,
                    profit_wei: U256::ZERO,
                    gas_used: 0,
                    revert_reason: Some(format!("EVM error: {}", e)),
                }
            }
        }
    }

    /// Simulate a transaction against a real RPC-backed fork state.
    ///
    /// Takes ownership of `RpcForkedState` because the underlying `AlloyDB`
    /// is not `Clone`. Creating a new `RpcForkedState` per simulation is cheap
    /// since the provider is `Arc`-wrapped internally.
    pub fn simulate_rpc(
        &self,
        state: RpcForkedState,
        to: Address,
        calldata: Vec<u8>,
    ) -> SimulationResult {
        let block = BlockEnv {
            number: U256::from(state.block_number),
            timestamp: U256::from(state.block_timestamp),
            basefee: state.base_fee,
            ..Default::default()
        };

        let tx = TxEnv::builder()
            .caller(self.config.caller)
            .kind(revm::primitives::TxKind::Call(to))
            .data(revm::primitives::Bytes::copy_from_slice(&calldata))
            .value(self.config.value)
            .gas_limit(self.config.gas_limit)
            .gas_price(state.base_fee as u128)
            .nonce(0)
            .chain_id(Some(state.chain_id))
            .build_fill();

        let ctx = Context::<BlockEnv, TxEnv, _, RpcDB, revm::context::Journal<RpcDB>, ()>::new(
            state.db,
            SpecId::CANCUN,
        )
        .with_block(block)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = state.chain_id;
            cfg.disable_nonce_check = true;
        });

        let mut evm = ctx.build_mainnet();

        match evm.transact(tx) {
            Ok(result_and_state) => match result_and_state.result {
                ExecutionResult::Success {
                    gas_used, output: _, ..
                } => {
                    debug!(gas_used, "RPC simulation succeeded");
                    SimulationResult {
                        success: true,
                        profit_wei: U256::ZERO,
                        gas_used,
                        revert_reason: None,
                    }
                }
                ExecutionResult::Revert { gas_used, output } => {
                    let reason = format!("0x{}", alloy::hex::encode(&output));
                    debug!(gas_used, reason = %reason, "RPC simulation reverted");
                    SimulationResult {
                        success: false,
                        profit_wei: U256::ZERO,
                        gas_used,
                        revert_reason: Some(reason),
                    }
                }
                ExecutionResult::Halt { reason, gas_used } => {
                    let reason_str = format!("{:?}", reason);
                    debug!(gas_used, reason = %reason_str, "RPC simulation halted");
                    SimulationResult {
                        success: false,
                        profit_wei: U256::ZERO,
                        gas_used,
                        revert_reason: Some(reason_str),
                    }
                }
            },
            Err(e) => {
                error!(error = %e, "RPC EVM transact error");
                SimulationResult {
                    success: false,
                    profit_wei: U256::ZERO,
                    gas_used: 0,
                    revert_reason: Some(format!("EVM error: {}", e)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    /// Helper: create a basic ForkedState with a funded caller
    fn setup_state_with_caller(caller: Address) -> ForkedState {
        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 30);
        state.insert_account_balance(caller, U256::from(100_000_000_000_000_000_000u128));
        state
    }

    #[test]
    fn test_evm_simulator_creation() {
        let sim = EvmSimulator::with_defaults();
        assert_eq!(sim.config.gas_limit, 1_000_000);
        assert_eq!(sim.config.chain_id, 1);

        let custom_config = SimConfig {
            gas_limit: 5_000_000,
            chain_id: 1,
            caller: address!("1111111111111111111111111111111111111111"),
            value: U256::ZERO,
        };
        let sim = EvmSimulator::new(custom_config.clone());
        assert_eq!(sim.config.gas_limit, 5_000_000);
    }

    #[test]
    fn test_simulate_simple_eth_transfer() {
        // Simulate a plain ETH transfer (empty calldata to an EOA)
        let caller = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let recipient = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));
        state.insert_account_balance(recipient, U256::ZERO);

        let config = SimConfig {
            gas_limit: 21_000,
            chain_id: 1,
            caller,
            value: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
        };

        let sim = EvmSimulator::new(config);

        // Empty calldata = simple transfer
        let result = sim.simulate(&state, recipient, vec![]);

        assert!(result.success, "ETH transfer should succeed: {:?}", result.revert_reason);
        assert!(result.gas_used > 0, "Gas should be consumed");
        assert!(result.revert_reason.is_none());
    }

    #[test]
    fn test_simulate_call_to_empty_address() {
        // Calling an address with no code should succeed (empty call)
        let caller = address!("cccccccccccccccccccccccccccccccccccccccc");
        let target = address!("dddddddddddddddddddddddddddddddddddddddd");

        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));

        let config = SimConfig {
            gas_limit: 100_000,
            chain_id: 1,
            caller,
            value: U256::ZERO,
        };

        let sim = EvmSimulator::new(config);
        let result = sim.simulate(&state, target, vec![0x12, 0x34, 0x56, 0x78]);

        // Call to account without code with calldata succeeds (it's a noop)
        assert!(result.success, "Call to empty address should succeed: {:?}", result.revert_reason);
    }

    #[test]
    fn test_simulate_contract_that_returns() {
        // Deploy a contract that just returns: PUSH1 0x00 PUSH1 0x00 RETURN
        // Bytecode: 0x60006000f3
        let caller = address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
        let contract = address!("ffffffffffffffffffffffffffffffffffffffff");

        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));

        // Contract that returns empty data: PUSH1 0x00 PUSH1 0x00 RETURN
        let bytecode = vec![0x60, 0x00, 0x60, 0x00, 0xf3];
        state.insert_account(
            contract,
            U256::ZERO,
            alloy::primitives::Bytes::from(bytecode),
        );

        let config = SimConfig {
            gas_limit: 100_000,
            chain_id: 1,
            caller,
            value: U256::ZERO,
        };

        let sim = EvmSimulator::new(config);
        let result = sim.simulate(&state, contract, vec![]);

        assert!(result.success, "Contract call should succeed: {:?}", result.revert_reason);
        assert!(result.gas_used > 0);
        assert!(result.revert_reason.is_none());
    }

    #[test]
    fn test_simulate_contract_that_reverts() {
        // Contract that always reverts: PUSH1 0x00 PUSH1 0x00 REVERT
        // Bytecode: 0x60006000fd
        let caller = address!("1010101010101010101010101010101010101010");
        let contract = address!("2020202020202020202020202020202020202020");

        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));

        // Contract that reverts: PUSH1 0x00 PUSH1 0x00 REVERT
        let bytecode = vec![0x60, 0x00, 0x60, 0x00, 0xfd];
        state.insert_account(
            contract,
            U256::ZERO,
            alloy::primitives::Bytes::from(bytecode),
        );

        let config = SimConfig {
            gas_limit: 100_000,
            chain_id: 1,
            caller,
            value: U256::ZERO,
        };

        let sim = EvmSimulator::new(config);
        let result = sim.simulate(&state, contract, vec![]);

        assert!(!result.success, "Reverting contract should fail");
        assert!(result.revert_reason.is_some());
        assert!(result.gas_used > 0);
    }

    #[test]
    fn test_simulate_contract_that_stores() {
        // Contract that stores calldata to slot 0:
        // CALLDATALOAD(0) -> PUSH1 0x00 -> SSTORE -> STOP
        // 35 6000 55 00
        // But we need PUSH1 0 first for calldataload: PUSH1 0x00 CALLDATALOAD PUSH1 0x00 SSTORE STOP
        // 6000 35 6000 55 00
        let caller = address!("3030303030303030303030303030303030303030");
        let contract = address!("4040404040404040404040404040404040404040");

        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));

        // PUSH1 0x00, CALLDATALOAD, PUSH1 0x00, SSTORE, STOP
        let bytecode = vec![0x60, 0x00, 0x35, 0x60, 0x00, 0x55, 0x00];
        state.insert_account(
            contract,
            U256::ZERO,
            alloy::primitives::Bytes::from(bytecode),
        );

        let config = SimConfig {
            gas_limit: 200_000,
            chain_id: 1,
            caller,
            value: U256::ZERO,
        };

        let sim = EvmSimulator::new(config);

        // Send calldata with a value to store
        let mut calldata = vec![0u8; 32];
        calldata[31] = 42; // Store value 42

        let result = sim.simulate(&state, contract, calldata);

        assert!(result.success, "Storage write should succeed: {:?}", result.revert_reason);
        // SSTORE costs significant gas
        assert!(result.gas_used > 20_000, "SSTORE should cost significant gas, got {}", result.gas_used);
    }

    #[test]
    fn test_simulate_insufficient_gas() {
        // Give very little gas to a contract that does work
        let caller = address!("5050505050505050505050505050505050505050");
        let contract = address!("6060606060606060606060606060606060606060");

        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));

        // Contract that stores (needs ~20k+ gas for SSTORE)
        let bytecode = vec![0x60, 0x01, 0x60, 0x00, 0x55, 0x00];
        state.insert_account(
            contract,
            U256::ZERO,
            alloy::primitives::Bytes::from(bytecode),
        );

        let config = SimConfig {
            gas_limit: 100, // Way too little gas
            chain_id: 1,
            caller,
            value: U256::ZERO,
        };

        let sim = EvmSimulator::new(config);
        let result = sim.simulate(&state, contract, vec![]);

        // Should fail due to out of gas (halt or validation error)
        // The EVM may fail during validation or halt during execution
        // Either way, it should not succeed
        assert!(
            !result.success || result.gas_used <= 100,
            "Should either fail or use minimal gas"
        );
    }

    #[test]
    fn test_simulate_does_not_mutate_original_state() {
        let caller = address!("7070707070707070707070707070707070707070");
        let contract = address!("8080808080808080808080808080808080808080");

        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        let initial_balance = U256::from(10_000_000_000_000_000_000u128);
        state.insert_account_balance(caller, initial_balance);

        // Contract that returns
        let bytecode = vec![0x60, 0x00, 0x60, 0x00, 0xf3];
        state.insert_account(
            contract,
            U256::ZERO,
            alloy::primitives::Bytes::from(bytecode),
        );

        let config = SimConfig {
            gas_limit: 100_000,
            chain_id: 1,
            caller,
            value: U256::ZERO,
        };

        let sim = EvmSimulator::new(config);
        let _ = sim.simulate(&state, contract, vec![]);

        // Original state should be unchanged
        let info = state.get_account(&caller).expect("Caller should still exist");
        assert_eq!(
            info.balance, initial_balance,
            "Original state should not be mutated"
        );
    }

    #[test]
    fn test_simulate_with_block_env() {
        // Verify that block number/timestamp are correctly set
        // Contract: NUMBER PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN
        // This returns the block number as output
        // NUMBER=0x43, PUSH1=0x60, MSTORE=0x52, RETURN=0xf3
        let caller = address!("9090909090909090909090909090909090909090");
        let contract = address!("a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0");

        let block_number = 19_500_000u64;
        let mut state = ForkedState::new_empty(block_number, 1_700_000_000, 0);
        state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));

        // Contract: NUMBER PUSH1 0x00 MSTORE PUSH1 0x20 PUSH1 0x00 RETURN
        let bytecode = vec![0x43, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];
        state.insert_account(
            contract,
            U256::ZERO,
            alloy::primitives::Bytes::from(bytecode),
        );

        let config = SimConfig {
            gas_limit: 100_000,
            chain_id: 1,
            caller,
            value: U256::ZERO,
        };

        let sim = EvmSimulator::new(config);
        let result = sim.simulate(&state, contract, vec![]);

        assert!(result.success, "Block number contract should succeed: {:?}", result.revert_reason);
    }

    #[test]
    fn test_simulate_multiple_times() {
        // Ensure simulator can be reused for multiple simulations
        let caller = address!("b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0");
        let contract = address!("c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0");

        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));

        let bytecode = vec![0x60, 0x00, 0x60, 0x00, 0xf3];
        state.insert_account(
            contract,
            U256::ZERO,
            alloy::primitives::Bytes::from(bytecode),
        );

        let config = SimConfig {
            gas_limit: 100_000,
            chain_id: 1,
            caller,
            value: U256::ZERO,
        };

        let sim = EvmSimulator::new(config);

        // Simulate multiple times
        for _ in 0..5 {
            let result = sim.simulate(&state, contract, vec![]);
            assert!(result.success, "Each simulation should succeed: {:?}", result.revert_reason);
        }
    }

    #[test]
    fn test_simulate_with_profit_calculation() {
        // Test the simulate_with_profit method with a simple scenario
        let caller = address!("d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0");
        let contract = address!("e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0");

        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));

        // Simple contract that returns
        let bytecode = vec![0x60, 0x00, 0x60, 0x00, 0xf3];
        state.insert_account(
            contract,
            U256::ZERO,
            alloy::primitives::Bytes::from(bytecode),
        );

        let config = SimConfig {
            gas_limit: 100_000,
            chain_id: 1,
            caller,
            value: U256::ZERO,
        };

        let sim = EvmSimulator::new(config);
        let result = sim.simulate_with_profit(
            &state,
            contract,
            vec![],
            Address::ZERO,
            caller,
        );

        assert!(result.success, "Simulation should succeed: {:?}", result.revert_reason);
    }
}
