// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {Math} from "@openzeppelin/contracts/utils/math/Math.sol";

interface IWETH {
    function deposit() external payable;
    function withdraw(uint256 wad) external;
    function transfer(address to, uint256 amount) external returns (bool);
}

/// @title AetherExecutor - Flash loan arbitrage executor
/// @notice Executes cross-DEX arbitrage using Aave V3 flash loans
/// @dev All swap steps must be profitable after gas + flash loan premium
contract AetherExecutor is Ownable2Step, ReentrancyGuard {
    using SafeERC20 for IERC20;

    address public immutable aavePool;

    /// @dev Canonical WETH address on Ethereum mainnet
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

    // Protocol constants matching ProtocolType enum in crates/common/src/types.rs
    uint8 constant UNISWAP_V2 = 1;
    uint8 constant UNISWAP_V3 = 2;
    uint8 constant SUSHISWAP = 3;
    uint8 constant CURVE = 4;
    uint8 constant BALANCER_V2 = 5;
    uint8 constant BANCOR_V3 = 6;

    /// @notice Runtime DEX registry — lets the owner swap router/vault addresses and
    ///         disable a compromised protocol without redeploying. Full redeploy is
    ///         still required to add a brand-new protocol *type* (new inline _swapX branch).
    mapping(uint8 => address) public protocolRouter;
    mapping(uint8 => bool) public protocolEnabled;

    /// @notice Circuit-breaker — when true, `executeArb` reverts. Flipped by the Go risk manager
    ///         when e.g. gas spikes above threshold or daily PnL crosses its floor.
    bool public paused;

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
    event DexRouterSet(uint8 indexed protocol, address router);
    event DexEnabledSet(uint8 indexed protocol, bool enabled);
    event PausedSet(bool paused);

    error NotAavePool();
    error InvalidInitiator();
    error FlashLoanFailed();
    error NotPendingV3Pool();
    error DeadlineExpired();
    error InsufficientProfit(uint256 actual, uint256 required);
    error InsufficientOutput(uint256 stepIndex, uint256 actual, uint256 expected);
    error ZeroAddress();
    error ArrayLengthMismatch();
    error SwapFailed(uint256 stepIndex);
    error TipBpsTooHigh();
    error CoinbaseTipFailed();
    error UnknownProtocol(uint8 protocol);
    error ProtocolDisabled(uint8 protocol);
    error ZeroRouter();
    error Paused();

    // UniswapV3 callback state — set before the swap call, validated in callback
    address private _pendingV3Pool;
    address private _pendingV3TokenIn;
    uint256 private _pendingV3AmountIn;

    modifier whenNotPaused() {
        if (paused) revert Paused();
        _;
    }

    constructor(address _aavePool, address _balancerVault, address _bancorNetwork)
        Ownable(msg.sender)
    {
        if (_aavePool == address(0)) revert ZeroAddress();
        if (_balancerVault == address(0)) revert ZeroAddress();
        if (_bancorNetwork == address(0)) revert ZeroAddress();
        aavePool = _aavePool;

        // Seed registry with mainnet defaults. UniV2/V3/Sushi/Curve use per-swap pool addresses
        // (no single router), so their entries stay at address(0) — `protocolRouter` is only
        // meaningful for Balancer (single Vault) and Bancor (single BancorNetwork).
        protocolRouter[BALANCER_V2] = _balancerVault;
        protocolRouter[BANCOR_V3] = _bancorNetwork;

        for (uint8 p = UNISWAP_V2; p <= BANCOR_V3; p++) {
            protocolEnabled[p] = true;
        }
    }

    // ─────────────────────────── DEX registry management ───────────────────────────

    /// @notice Replace the router/vault address for a protocol (e.g. Balancer Vault migration).
    /// @dev Only valid for BALANCER_V2 and BANCOR_V3 in the current implementation; the
    ///      per-swap-pool protocols keep address(0) here.
    function setDexRouter(uint8 protocol, address router) external onlyOwner {
        if (protocol == 0 || protocol > BANCOR_V3) revert UnknownProtocol(protocol);
        if (router == address(0)) revert ZeroRouter();
        protocolRouter[protocol] = router;
        emit DexRouterSet(protocol, router);
    }

    /// @notice Per-protocol kill switch. Idempotent — no event on no-op writes.
    function setDexEnabled(uint8 protocol, bool enabled) external onlyOwner {
        if (protocol == 0 || protocol > BANCOR_V3) revert UnknownProtocol(protocol);
        if (protocolEnabled[protocol] == enabled) return;
        protocolEnabled[protocol] = enabled;
        emit DexEnabledSet(protocol, enabled);
    }

    /// @notice Global pause — flipped by the Go risk manager on circuit-breaker trip.
    function setPaused(bool _paused) external onlyOwner {
        if (paused == _paused) return;
        paused = _paused;
        emit PausedSet(_paused);
    }

    // ─────────────────────────── Arb execution ───────────────────────────

    /// @notice Entry point - initiates flash loan and arb execution
    /// @param steps Array of swap steps to execute
    /// @param flashloanToken Token to borrow
    /// @param flashloanAmount Amount to borrow
    /// @param deadline Unix timestamp after which the transaction reverts
    /// @param minProfitOut Minimum profit required after flash loan repayment (slippage backstop)
    /// @param tipBps Tip to block.coinbase in basis points (e.g. 9000 = 90%)
    function executeArb(
        SwapStep[] calldata steps,
        address flashloanToken,
        uint256 flashloanAmount,
        uint256 deadline,
        uint256 minProfitOut,
        uint256 tipBps
    ) external onlyOwner nonReentrant whenNotPaused {
        if (block.timestamp > deadline) revert DeadlineExpired();
        if (tipBps > 10_000) revert TipBpsTooHigh();

        // CRITICAL: validate every step's protocol is enabled BEFORE starting the flash loan.
        // A disabled flag discovered mid-callback would burn the full tx gas (hop N already
        // swapped, flashloan premium owed). Pre-flight check = ≤1 SLOAD per hop, fails fast.
        uint256 stepsLen = steps.length;
        for (uint256 i = 0; i < stepsLen;) {
            uint8 p = steps[i].protocol;
            if (p == 0 || p > BANCOR_V3) revert UnknownProtocol(p);
            if (!protocolEnabled[p]) revert ProtocolDisabled(p);
            unchecked { ++i; }
        }

        // Encode steps, tip config, and profit floor for callback.
        // gasStart moved into executeOperation so the measured gasUsed reflects on-chain work,
        // not the calldata encode cost above.
        bytes memory params = abi.encode(steps, tipBps, minProfitOut);

        // Initiate flash loan - Aave V3 IPool.flashLoanSimple
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
    /// @dev Called by Aave pool after sending the borrowed funds.
    ///      nonReentrant is intentionally NOT applied here — this function is
    ///      called by Aave within the same tx initiated by executeArb(), and
    ///      the reentrancy guard on executeArb() would deadlock if applied here.
    /// @param asset The borrowed token address
    /// @param amount The borrowed amount
    /// @param premium The flash loan fee
    /// @param initiator The address that initiated the flash loan (must be this contract)
    /// @param params Encoded swap steps, tip config, and profit floor
    /// @return True on success
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external returns (bool) {
        // Snapshot gas at the top of the on-chain execution path so the emitted gasUsed
        // reflects only on-chain work, not the calldata build-up in executeArb.
        uint256 gasStart = gasleft();

        if (msg.sender != aavePool) revert NotAavePool();
        if (initiator != address(this)) revert InvalidInitiator();

        (SwapStep[] memory steps, uint256 tipBps, uint256 minProfitOut) =
            abi.decode(params, (SwapStep[], uint256, uint256));

        // Execute all swap steps
        uint256 len = steps.length;
        for (uint256 i = 0; i < len;) {
            _executeSwap(steps[i], i);
            unchecked { ++i; }
        }

        // Repay flash loan and distribute profit
        (uint256 profit, uint256 tipAmount) = _repayAndDistribute(asset, amount, premium, tipBps, minProfitOut);

        uint256 gasUsed = gasStart - gasleft();
        emit ArbExecuted(asset, amount, profit, tipAmount, gasUsed);

        return true;
    }

    /// @dev Repay flash loan, enforce profit floor, split profit between coinbase tip and owner
    /// @return profit Total profit before tip/owner split
    /// @return tipAmount Amount sent to block.coinbase
    function _repayAndDistribute(
        address asset,
        uint256 amount,
        uint256 premium,
        uint256 tipBps,
        uint256 minProfitOut
    ) internal returns (uint256 profit, uint256 tipAmount) {
        uint256 totalDebt = amount + premium;

        // Fallback: ensure Aave pool has sufficient allowance for repayment.
        if (IERC20(asset).allowance(address(this), aavePool) < totalDebt) {
            IERC20(asset).forceApprove(aavePool, type(uint256).max);
        }

        uint256 balance = IERC20(asset).balanceOf(address(this));
        if (balance <= totalDebt) revert InsufficientProfit(0, minProfitOut);
        profit = balance - totalDebt;

        if (profit < minProfitOut) revert InsufficientProfit(profit, minProfitOut);

        tipAmount = (profit * tipBps) / 10_000;
        uint256 ownerProfit;
        unchecked { ownerProfit = profit - tipAmount; }

        if (tipAmount > 0) {
            if (asset == WETH) {
                // Unwrap WETH, then try native ETH transfer; on failure re-wrap and send as WETH.
                // Some builders run contract coinbases that reject plain ETH transfers.
                IWETH(asset).withdraw(tipAmount);
                (bool sent,) = block.coinbase.call{value: tipAmount}("");
                if (!sent) {
                    IWETH(WETH).deposit{value: tipAmount}();
                    IERC20(WETH).safeTransfer(block.coinbase, tipAmount);
                }
            } else {
                // Non-WETH fallback: ERC-20 transfer (builders won't prioritize)
                IERC20(asset).safeTransfer(block.coinbase, tipAmount);
            }
        }
        if (ownerProfit > 0) {
            IERC20(asset).safeTransfer(owner(), ownerProfit);
        }
    }

    /// @notice Pre-approve spenders to save gas during arb execution
    function setApprovals(address[] calldata tokens, address[] calldata spenders) external onlyOwner {
        if (tokens.length != spenders.length) revert ArrayLengthMismatch();
        uint256 len = tokens.length;
        for (uint256 i = 0; i < len;) {
            if (spenders[i] == address(0)) revert ZeroAddress();
            IERC20(tokens[i]).forceApprove(spenders[i], type(uint256).max);
            unchecked { ++i; }
        }
    }

    /// @dev Execute a single swap step based on protocol.
    function _executeSwap(SwapStep memory step, uint256 index) internal {
        // Defense-in-depth: the pre-flight check in executeArb already rejected disabled
        // protocols, but future internal callers (e.g. direct-call paths) must also be guarded.
        if (!protocolEnabled[step.protocol]) revert ProtocolDisabled(step.protocol);

        uint256 balanceBefore = IERC20(step.tokenOut).balanceOf(address(this));

        // Hot-path first: UniV2 and UniV3 are by far the most common protocols
        if (step.protocol == UNISWAP_V2) {
            // Cap the transfer at our actual balance — protects against off-chain optimizer
            // over-spec'ing amountIn vs. what's really available post-prior-hop.
            uint256 actualIn = Math.min(step.amountIn, IERC20(step.tokenIn).balanceOf(address(this)));
            IERC20(step.tokenIn).safeTransfer(step.pool, actualIn);
            _swapUniV2(step, index);
        } else if (step.protocol == UNISWAP_V3) {
            _swapUniV3(step, index);
        } else if (step.protocol == SUSHISWAP) {
            uint256 actualIn = Math.min(step.amountIn, IERC20(step.tokenIn).balanceOf(address(this)));
            IERC20(step.tokenIn).safeTransfer(step.pool, actualIn);
            _swapUniV2(step, index);
        } else if (step.protocol == CURVE) {
            _swapCurve(step, index);
        } else if (step.protocol == BALANCER_V2) {
            _swapBalancer(step, index);
        } else if (step.protocol == BANCOR_V3) {
            _swapBancor(step, index);
        } else {
            revert UnknownProtocol(step.protocol);
        }

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

        _pendingV3Pool = address(0);
        _pendingV3TokenIn = address(0);
        _pendingV3AmountIn = 0;
    }

    /// @notice UniswapV3 swap callback — called by the pool during swap to collect tokenIn
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        if (msg.sender != _pendingV3Pool) revert NotPendingV3Pool();

        require(amount0Delta > 0 || amount1Delta > 0, "V3: no amount owed");
        uint256 amountOwed = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        if (amountOwed > _pendingV3AmountIn) amountOwed = _pendingV3AmountIn;
        _pendingV3AmountIn = 0;

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

    /// @dev Balancer V2: approve the registry-configured Vault to pull tokens.
    function _swapBalancer(SwapStep memory step, uint256 index) internal {
        address vault = protocolRouter[BALANCER_V2];
        if (vault == address(0)) revert ZeroRouter();
        IERC20(step.tokenIn).forceApprove(vault, step.amountIn);
        (bool success,) = vault.call(step.data);
        if (!success) revert SwapFailed(index);
        IERC20(step.tokenIn).forceApprove(vault, 0);
    }

    /// @dev Bancor V3: approve the registry-configured BancorNetwork to pull tokens.
    function _swapBancor(SwapStep memory step, uint256 index) internal {
        address network = protocolRouter[BANCOR_V3];
        if (network == address(0)) revert ZeroRouter();
        IERC20(step.tokenIn).forceApprove(network, step.amountIn);
        (bool success,) = network.call(step.data);
        if (!success) revert SwapFailed(index);
        IERC20(step.tokenIn).forceApprove(network, 0);
    }

    /// @notice Emergency rescue - owner only. token==address(0) rescues native ETH.
    function rescue(address token, uint256 amount) external onlyOwner {
        if (token == address(0)) {
            (bool ok,) = owner().call{value: amount}("");
            require(ok, "ETH rescue failed");
        } else {
            IERC20(token).safeTransfer(owner(), amount);
        }
    }

    /// @notice Accept ETH (needed for WETH unwrap during coinbase tip)
    receive() external payable {}
}
