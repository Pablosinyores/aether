// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IERC20} from "forge-std/interfaces/IERC20.sol";

/// @title AetherExecutor - Flash loan arbitrage executor
/// @notice Executes cross-DEX arbitrage using Aave V3 flash loans
/// @dev All swap steps must be profitable after gas + flash loan premium
contract AetherExecutor {
    address public owner;
    address public immutable aavePool;

    // Protocol constants matching ProtocolType enum
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
    error InsufficientProfit();
    error SwapFailed(uint256 stepIndex);
    error InsufficientOutput(uint256 stepIndex, uint256 expected, uint256 actual);

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
        require(success, "Flash loan failed");
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
        require(initiator == address(this), "Invalid initiator");

        (SwapStep[] memory steps, uint256 gasStart) = abi.decode(params, (SwapStep[], uint256));

        // Execute all swap steps
        for (uint256 i = 0; i < steps.length; i++) {
            _executeSwap(steps[i], i);
        }

        // Repay flash loan (amount + premium)
        uint256 totalDebt = amount + premium;
        IERC20(asset).approve(aavePool, totalDebt);

        // Calculate and transfer profit
        uint256 balance = IERC20(asset).balanceOf(address(this));
        if (balance <= totalDebt) revert InsufficientProfit();
        uint256 profit = balance - totalDebt;

        // Transfer profit to owner
        IERC20(asset).transfer(owner, profit);

        uint256 gasUsed = gasStart - gasleft();
        emit ArbExecuted(asset, amount, profit, gasUsed);

        return true;
    }

    /// @dev Execute a single swap step based on protocol
    function _executeSwap(SwapStep memory step, uint256 index) internal {
        // Transfer tokens to pool (for protocols that require it)
        IERC20(step.tokenIn).transfer(step.pool, step.amountIn);

        uint256 balanceBefore = IERC20(step.tokenOut).balanceOf(address(this));

        if (step.protocol == UNISWAP_V2 || step.protocol == SUSHISWAP) {
            _swapUniV2(step);
        } else if (step.protocol == UNISWAP_V3) {
            _swapUniV3(step);
        } else if (step.protocol == CURVE) {
            _swapCurve(step);
        } else if (step.protocol == BALANCER_V2) {
            _swapBalancer(step);
        } else if (step.protocol == BANCOR_V3) {
            _swapBancor(step);
        } else {
            revert SwapFailed(index);
        }

        uint256 balanceAfter = IERC20(step.tokenOut).balanceOf(address(this));
        uint256 amountOut = balanceAfter - balanceBefore;
        if (amountOut < step.minAmountOut) {
            revert InsufficientOutput(index, step.minAmountOut, amountOut);
        }
    }

    function _swapUniV2(SwapStep memory step) internal {
        // UniswapV2Pair.swap(uint amount0Out, uint amount1Out, address to, bytes data)
        // We already transferred tokenIn, now call swap
        (bool success,) = step.pool.call(step.data);
        require(success, "UniV2 swap failed");
    }

    function _swapUniV3(SwapStep memory step) internal {
        (bool success,) = step.pool.call(step.data);
        require(success, "UniV3 swap failed");
    }

    function _swapCurve(SwapStep memory step) internal {
        (bool success,) = step.pool.call(step.data);
        require(success, "Curve swap failed");
    }

    function _swapBalancer(SwapStep memory step) internal {
        (bool success,) = step.pool.call(step.data);
        require(success, "Balancer swap failed");
    }

    function _swapBancor(SwapStep memory step) internal {
        (bool success,) = step.pool.call(step.data);
        require(success, "Bancor swap failed");
    }

    /// @notice Emergency token rescue - owner only
    function rescue(address token, uint256 amount) external onlyOwner {
        IERC20(token).transfer(owner, amount);
    }

    /// @notice Transfer ownership
    function transferOwnership(address newOwner) external onlyOwner {
        require(newOwner != address(0), "Zero address");
        owner = newOwner;
    }

    /// @notice Accept ETH
    receive() external payable {}
}
