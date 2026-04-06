// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @title AetherExecutor - Flash loan arbitrage executor
/// @notice Executes cross-DEX arbitrage using Aave V3 flash loans
/// @dev All swap steps must be profitable after gas + flash loan premium
contract AetherExecutor {
    using SafeERC20 for IERC20;

    address public owner;
    address public immutable aavePool;

    // Protocol constants matching ProtocolType enum in crates/common/src/types.rs
    uint8 constant UNISWAP_V2 = 1;
    uint8 constant UNISWAP_V3 = 2;
    uint8 constant SUSHISWAP = 3;
    uint8 constant CURVE = 4;
    uint8 constant BALANCER_V2 = 5;
    uint8 constant BANCOR_V3 = 6;

    struct SwapStep {
        uint8 protocol;
        address pool;
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint256 minAmountOut;
        bytes data; // Protocol-specific calldata
    }

    event ArbExecuted(
        address indexed flashloanToken,
        uint256 flashloanAmount,
        uint256 profit,
        uint256 gasUsed
    );

    error NotOwner();
    error NotAavePool();
    error InvalidInitiator();
    error FlashLoanFailed();
    error InsufficientProfit();
    error ZeroAddress();
    error SwapFailed(uint256 stepIndex);

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(address _aavePool) {
        owner = msg.sender;
        aavePool = _aavePool;
    }

    /// @notice Entry point - initiates flash loan and arb execution
    /// @param steps Array of swap steps to execute
    /// @param flashloanToken Token to borrow
    /// @param flashloanAmount Amount to borrow
    function executeArb(
        SwapStep[] calldata steps,
        address flashloanToken,
        uint256 flashloanAmount
    ) external onlyOwner {
        uint256 gasStart = gasleft();

        // Encode steps for callback
        bytes memory params = abi.encode(steps, gasStart);

        // Initiate flash loan - Aave V3 IPool.flashLoanSimple
        // function flashLoanSimple(address receiverAddress, address asset, uint256 amount, bytes calldata params, uint16 referralCode)
        (bool success,) = aavePool.call(
            abi.encodeWithSignature(
                "flashLoanSimple(address,address,uint256,bytes,uint16)",
                address(this),
                flashloanToken,
                flashloanAmount,
                params,
                uint16(0)
            )
        );
        if (!success) revert FlashLoanFailed();
    }

    /// @notice Aave V3 flash loan callback
    /// @dev Called by Aave pool after sending the borrowed funds
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external returns (bool) {
        if (msg.sender != aavePool) revert NotAavePool();
        if (initiator != address(this)) revert InvalidInitiator();

        (SwapStep[] memory steps, uint256 gasStart) = abi.decode(params, (SwapStep[], uint256));

        // Execute all swap steps
        uint256 len = steps.length;
        for (uint256 i = 0; i < len;) {
            _executeSwap(steps[i], i);
            // Safe: i < len, so i+1 cannot overflow
            unchecked { ++i; }
        }

        // Repay flash loan (amount + premium); approvals must be pre-set via setApprovals()
        uint256 totalDebt = amount + premium;

        // Calculate and transfer profit
        uint256 balance = IERC20(asset).balanceOf(address(this));
        if (balance <= totalDebt) revert InsufficientProfit();
        uint256 profit = balance - totalDebt;

        // Transfer profit to owner
        IERC20(asset).safeTransfer(owner, profit);

        uint256 gasUsed = gasStart - gasleft();
        emit ArbExecuted(asset, amount, profit, gasUsed);

        return true;
    }

    /// @notice Pre-approve spenders to save gas during arb execution
    /// @dev Call once per token/spender pair (e.g., flashloan token → Aave pool).
    ///      Uses max approval so executeOperation never needs to re-approve.
    /// @param tokens ERC20 token addresses to approve
    /// @param spenders Corresponding spender addresses (must be same length as tokens)
    function setApprovals(address[] calldata tokens, address[] calldata spenders) external onlyOwner {
        uint256 len = tokens.length;
        for (uint256 i = 0; i < len;) {
            IERC20(tokens[i]).forceApprove(spenders[i], type(uint256).max);
            // Safe: i < len, so i+1 cannot overflow
            unchecked { ++i; }
        }
    }

    /// @dev Execute a single swap step based on protocol.
    ///      Final profit validation in executeOperation ensures no net loss;
    ///      per-step balanceOf checks are omitted to save ~5.2K gas per hop.
    function _executeSwap(SwapStep memory step, uint256 index) internal {
        // Transfer tokens to pool (for protocols that require it)
        IERC20(step.tokenIn).safeTransfer(step.pool, step.amountIn);

        // Hot-path first: UniV2 and UniV3 are by far the most common protocols
        if (step.protocol == UNISWAP_V2) {
            _swapUniV2(step, index);
        } else if (step.protocol == UNISWAP_V3) {
            _swapUniV3(step, index);
        } else if (step.protocol == SUSHISWAP) {
            _swapUniV2(step, index); // SushiSwap uses the same AMM interface as UniV2
        } else if (step.protocol == CURVE) {
            _swapCurve(step, index);
        } else if (step.protocol == BALANCER_V2) {
            _swapBalancer(step, index);
        } else if (step.protocol == BANCOR_V3) {
            _swapBancor(step, index);
        } else {
            revert SwapFailed(index);
        }
    }

    /// @dev UniswapV2 (and SushiSwap) swap. Token already transferred to pool.
    function _swapUniV2(SwapStep memory step, uint256 index) internal {
        // UniswapV2Pair.swap(uint amount0Out, uint amount1Out, address to, bytes data)
        (bool success,) = step.pool.call(step.data);
        if (!success) revert SwapFailed(index);
    }

    /// @dev UniswapV3 swap.
    function _swapUniV3(SwapStep memory step, uint256 index) internal {
        (bool success,) = step.pool.call(step.data);
        if (!success) revert SwapFailed(index);
    }

    /// @dev Curve exchange swap.
    function _swapCurve(SwapStep memory step, uint256 index) internal {
        (bool success,) = step.pool.call(step.data);
        if (!success) revert SwapFailed(index);
    }

    /// @dev Balancer V2 swap.
    function _swapBalancer(SwapStep memory step, uint256 index) internal {
        (bool success,) = step.pool.call(step.data);
        if (!success) revert SwapFailed(index);
    }

    /// @dev Bancor V3 swap.
    function _swapBancor(SwapStep memory step, uint256 index) internal {
        (bool success,) = step.pool.call(step.data);
        if (!success) revert SwapFailed(index);
    }

    /// @notice Emergency token rescue - owner only
    /// @param token ERC20 token to rescue
    /// @param amount Amount to rescue
    function rescue(address token, uint256 amount) external onlyOwner {
        IERC20(token).safeTransfer(owner, amount);
    }

    /// @notice Transfer contract ownership
    /// @param newOwner Address of new owner; must be non-zero
    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        owner = newOwner;
    }

    /// @notice Accept ETH
    receive() external payable {}
}
