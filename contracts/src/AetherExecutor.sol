// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

interface IWETH {
    function withdraw(uint256 wad) external;
}

/// @title AetherExecutor - Flash loan arbitrage executor
/// @notice Executes cross-DEX arbitrage using Aave V3 flash loans
/// @dev All swap steps must be profitable after gas + flash loan premium
contract AetherExecutor is ReentrancyGuard {
    using SafeERC20 for IERC20;

    address public owner;
    address public immutable aavePool;
    address public immutable balancerVault;
    address public immutable bancorNetwork;

    /// @dev Canonical WETH address on Ethereum mainnet
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

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
        uint256 tipAmount,
        uint256 gasUsed
    );

    error NotOwner();
    error NotAavePool();
    error InvalidInitiator();
    error FlashLoanFailed();
    error NotPendingV3Pool();
    error InsufficientProfit();
    error InsufficientOutput(uint256 stepIndex, uint256 actual, uint256 expected);
    error ZeroAddress();
    error ArrayLengthMismatch();
    error SwapFailed(uint256 stepIndex);
    error TipBpsTooHigh();
    error CoinbaseTipFailed();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    // UniswapV3 callback state — set before the swap call, validated in callback
    address private _pendingV3Pool;
    address private _pendingV3TokenIn;
    uint256 private _pendingV3AmountIn;

    constructor(address _aavePool, address _balancerVault, address _bancorNetwork) {
        require(_aavePool != address(0), "Zero aavePool");
        require(_balancerVault != address(0), "Zero balancerVault");
        require(_bancorNetwork != address(0), "Zero bancorNetwork");
        owner = msg.sender;
        aavePool = _aavePool;
        balancerVault = _balancerVault;
        bancorNetwork = _bancorNetwork;
    }

    /// @notice Entry point - initiates flash loan and arb execution
    /// @param steps Array of swap steps to execute
    /// @param flashloanToken Token to borrow
    /// @param flashloanAmount Amount to borrow
    /// @param tipBps Tip to block.coinbase in basis points (e.g. 9000 = 90%)
    function executeArb(
        SwapStep[] calldata steps,
        address flashloanToken,
        uint256 flashloanAmount,
        uint256 tipBps
    ) external onlyOwner {
        if (tipBps > 10_000) revert TipBpsTooHigh();

        uint256 gasStart = gasleft();

        // Encode steps, gas snapshot, and tip config for callback
        bytes memory params = abi.encode(steps, gasStart, tipBps);

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
    ) external nonReentrant returns (bool) {
        if (msg.sender != aavePool) revert NotAavePool();
        if (initiator != address(this)) revert InvalidInitiator();

        (SwapStep[] memory steps, uint256 gasStart, uint256 tipBps) =
            abi.decode(params, (SwapStep[], uint256, uint256));

        // Execute all swap steps
        uint256 len = steps.length;
        for (uint256 i = 0; i < len;) {
            _executeSwap(steps[i], i);
            // Safe: i < len, so i+1 cannot overflow
            unchecked { ++i; }
        }

        // Repay flash loan and distribute profit
        (uint256 profit, uint256 tipAmount) = _repayAndDistribute(asset, amount, premium, tipBps);

        uint256 gasUsed = gasStart - gasleft();
        emit ArbExecuted(asset, amount, profit, tipAmount, gasUsed);

        return true;
    }

    /// @dev Repay flash loan, split profit between coinbase tip and owner
    /// @return profit Total profit before tip/owner split
    /// @return tipAmount Amount sent to block.coinbase
    function _repayAndDistribute(
        address asset,
        uint256 amount,
        uint256 premium,
        uint256 tipBps
    ) internal returns (uint256 profit, uint256 tipAmount) {
        uint256 totalDebt = amount + premium;

        // Fallback: ensure Aave pool has sufficient allowance for repayment.
        // Pre-set via setApprovals() for gas savings; this covers new tokens.
        if (IERC20(asset).allowance(address(this), aavePool) < totalDebt) {
            IERC20(asset).forceApprove(aavePool, type(uint256).max);
        }

        uint256 balance = IERC20(asset).balanceOf(address(this));
        if (balance <= totalDebt) revert InsufficientProfit();
        profit = balance - totalDebt;

        // tipBps validated in executeArb (<=10000), so multiplication is safe
        tipAmount = (profit * tipBps) / 10_000;
        // Safe: tipAmount <= profit because tipBps <= 10000
        uint256 ownerProfit;
        unchecked { ownerProfit = profit - tipAmount; }

        if (tipAmount > 0) {
            if (asset == WETH) {
                // Unwrap WETH to native ETH so builders recognize the coinbase tip
                IWETH(asset).withdraw(tipAmount);
                (bool sent,) = block.coinbase.call{value: tipAmount}("");
                if (!sent) revert CoinbaseTipFailed();
            } else {
                // Non-WETH fallback: ERC-20 transfer (builders won't prioritize)
                IERC20(asset).safeTransfer(block.coinbase, tipAmount);
            }
        }
        if (ownerProfit > 0) {
            IERC20(asset).safeTransfer(owner, ownerProfit);
        }
    }

    /// @notice Pre-approve spenders to save gas during arb execution
    /// @dev Call once per token/spender pair (e.g., flashloan token → Aave pool).
    ///      Uses max approval so executeOperation never needs to re-approve.
    /// @param tokens ERC20 token addresses to approve
    /// @param spenders Corresponding spender addresses (must be same length as tokens)
    function setApprovals(address[] calldata tokens, address[] calldata spenders) external onlyOwner {
        if (tokens.length != spenders.length) revert ArrayLengthMismatch();
        uint256 len = tokens.length;
        for (uint256 i = 0; i < len;) {
            if (spenders[i] == address(0)) revert ZeroAddress();
            IERC20(tokens[i]).forceApprove(spenders[i], type(uint256).max);
            // Safe: i < len, so i+1 cannot overflow
            unchecked { ++i; }
        }
    }

    /// @dev Execute a single swap step based on protocol.
    ///      Each protocol handler manages its own token transfer pattern.
    ///      Per-step slippage protection via minAmountOut enforced after each swap.
    function _executeSwap(SwapStep memory step, uint256 index) internal {
        uint256 balanceBefore = IERC20(step.tokenOut).balanceOf(address(this));

        // Hot-path first: UniV2 and UniV3 are by far the most common protocols
        if (step.protocol == UNISWAP_V2) {
            // UniV2: pre-transfer tokens to pool, then call swap
            IERC20(step.tokenIn).safeTransfer(step.pool, step.amountIn);
            _swapUniV2(step, index);
        } else if (step.protocol == UNISWAP_V3) {
            // UniV3: pool calls back uniswapV3SwapCallback to pull tokens
            _swapUniV3(step, index);
        } else if (step.protocol == SUSHISWAP) {
            // Sushi: same pre-transfer pattern as UniV2
            IERC20(step.tokenIn).safeTransfer(step.pool, step.amountIn);
            _swapUniV2(step, index);
        } else if (step.protocol == CURVE) {
            // Curve: approve then pool pulls via transferFrom
            _swapCurve(step, index);
        } else if (step.protocol == BALANCER_V2) {
            // Balancer: approve Vault then Vault pulls via transferFrom
            _swapBalancer(step, index);
        } else if (step.protocol == BANCOR_V3) {
            // Bancor: approve then router pulls via transferFrom
            _swapBancor(step, index);
        } else {
            revert SwapFailed(index);
        }

        // Per-step slippage protection: verify minimum output received
        uint256 amountOut = IERC20(step.tokenOut).balanceOf(address(this)) - balanceBefore;
        if (amountOut < step.minAmountOut) {
            revert InsufficientOutput(index, amountOut, step.minAmountOut);
        }
    }

    /// @dev UniswapV2 (and SushiSwap) swap. Token already transferred to pool.
    function _swapUniV2(SwapStep memory step, uint256 index) internal {
        (bool success,) = step.pool.call(step.data);
        if (!success) revert SwapFailed(index);
    }

    /// @dev UniV3: set callback state, call pool.swap(), pool calls back uniswapV3SwapCallback
    function _swapUniV3(SwapStep memory step, uint256 index) internal {
        _pendingV3Pool = step.pool;
        _pendingV3TokenIn = step.tokenIn;
        _pendingV3AmountIn = step.amountIn;

        (bool success,) = step.pool.call(step.data);
        if (!success) revert SwapFailed(index);

        // Clear callback state
        _pendingV3Pool = address(0);
        _pendingV3TokenIn = address(0);
        _pendingV3AmountIn = 0;
    }

    /// @notice UniswapV3 swap callback — called by the pool during swap to collect tokenIn
    /// @param amount0Delta Amount of token0 owed (positive = owed by caller)
    /// @param amount1Delta Amount of token1 owed (positive = owed by caller)
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        if (msg.sender != _pendingV3Pool) revert NotPendingV3Pool();

        // At least one delta must be positive (amount owed to the pool)
        require(amount0Delta > 0 || amount1Delta > 0, "V3: no amount owed");
        // Transfer the owed amount to the pool (whichever delta is positive)
        uint256 amountOwed = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        // Cap at our expected amount to prevent a malicious pool from draining extra tokens
        if (amountOwed > _pendingV3AmountIn) amountOwed = _pendingV3AmountIn;
        // Zero out to prevent double-spend if pool calls back multiple times
        _pendingV3AmountIn = 0;

        // Skip transfer if nothing owed (e.g. second callback after double-spend protection)
        // Some ERC20s revert on zero-amount transfers
        if (amountOwed == 0) return;

        IERC20(_pendingV3TokenIn).safeTransfer(msg.sender, amountOwed);
    }

    /// @dev Curve: approve pool to pull tokens, call exchange, reset approval
    function _swapCurve(SwapStep memory step, uint256 index) internal {
        IERC20(step.tokenIn).forceApprove(step.pool, step.amountIn);
        (bool success,) = step.pool.call(step.data);
        if (!success) revert SwapFailed(index);
        IERC20(step.tokenIn).forceApprove(step.pool, 0);
    }

    /// @dev Balancer V2: approve the single Vault (not the pool) to pull tokens
    function _swapBalancer(SwapStep memory step, uint256 index) internal {
        IERC20(step.tokenIn).forceApprove(balancerVault, step.amountIn);
        (bool success,) = balancerVault.call(step.data);
        if (!success) revert SwapFailed(index);
        IERC20(step.tokenIn).forceApprove(balancerVault, 0);
    }

    /// @dev Bancor V3: approve the BancorNetwork contract (not the individual pool) to pull tokens
    /// @dev Individual Bancor pool contracts do not implement tradeBySourceAmount — all trades
    ///      must go through the single BancorNetwork router at the immutable bancorNetwork address.
    function _swapBancor(SwapStep memory step, uint256 index) internal {
        IERC20(step.tokenIn).forceApprove(bancorNetwork, step.amountIn);
        (bool success,) = bancorNetwork.call(step.data);
        if (!success) revert SwapFailed(index);
        IERC20(step.tokenIn).forceApprove(bancorNetwork, 0);
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
