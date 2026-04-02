// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {AetherExecutor} from "../src/AetherExecutor.sol";

/// @dev Mock ERC20 for testing
contract MockERC20 {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        require(balanceOf[msg.sender] >= amount, "Insufficient balance");
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        require(balanceOf[from] >= amount, "Insufficient balance");
        require(allowance[from][msg.sender] >= amount, "Insufficient allowance");
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        allowance[from][msg.sender] -= amount;
        return true;
    }
}

/// @dev Mock Aave pool that simulates flashLoanSimple callback flow
contract MockAavePool {
    function flashLoanSimple(
        address receiver,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 /* referralCode */
    ) external {
        // Simulate: send borrowed funds to receiver
        MockERC20(asset).mint(receiver, amount);

        // Aave V3 premium: 0.05% (5 bps)
        uint256 premium = (amount * 5) / 10000;

        // Call executeOperation on the receiver (as Aave pool would)
        AetherExecutor(payable(receiver)).executeOperation(
            asset,
            amount,
            premium,
            receiver, // initiator = the executor itself
            params
        );

        // Verify repayment: Aave would pull totalDebt via transferFrom
        uint256 totalDebt = amount + premium;
        MockERC20(asset).transferFrom(receiver, address(this), totalDebt);
    }
}

/// @dev Mock swap pool that simulates a profitable swap by minting extra tokens
contract MockSwapPool {
    address public tokenOut;
    uint256 public outAmount;

    constructor(address _tokenOut, uint256 _outAmount) {
        tokenOut = _tokenOut;
        outAmount = _outAmount;
    }

    fallback() external {
        // Simulate profitable swap: mint outAmount of tokenOut to caller
        MockERC20(tokenOut).mint(msg.sender, outAmount);
    }
}

contract AetherExecutorTest is Test {
    // Re-declare event for vm.expectEmit usage
    event ArbExecuted(
        address indexed flashloanToken,
        uint256 flashloanAmount,
        uint256 profit,
        uint256 tipAmount,
        uint256 gasUsed
    );

    AetherExecutor executor;
    MockERC20 token;
    address constant AAVE_POOL = address(0xAA);
    address owner;

    function setUp() public {
        owner = address(this);
        executor = new AetherExecutor(AAVE_POOL);
        token = new MockERC20();
    }

    function test_owner() public view {
        assertEq(executor.owner(), owner);
    }

    function test_aavePool() public view {
        assertEq(executor.aavePool(), AAVE_POOL);
    }

    function test_transferOwnership() public {
        address newOwner = address(0x123);
        executor.transferOwnership(newOwner);
        assertEq(executor.owner(), newOwner);
    }

    function test_transferOwnership_revert_notOwner() public {
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.transferOwnership(address(0x789));
    }

    function test_transferOwnership_revert_zeroAddress() public {
        vm.expectRevert("Zero address");
        executor.transferOwnership(address(0));
    }

    function test_rescue() public {
        // Mint tokens to executor
        token.mint(address(executor), 1000);
        assertEq(token.balanceOf(address(executor)), 1000);

        // Rescue tokens
        executor.rescue(address(token), 1000);
        assertEq(token.balanceOf(owner), 1000);
        assertEq(token.balanceOf(address(executor)), 0);
    }

    function test_rescue_revert_notOwner() public {
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.rescue(address(token), 100);
    }

    function test_executeArb_revert_notOwner() public {
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.executeArb(steps, address(token), 1000, 9000);
    }

    function test_receive_eth() public {
        vm.deal(address(this), 1 ether);
        (bool success,) = address(executor).call{value: 0.5 ether}("");
        assertTrue(success);
        assertEq(address(executor).balance, 0.5 ether);
    }

    // --- New tipBps tests ---

    function test_tipBps_tooHigh() public {
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.expectRevert(AetherExecutor.TipBpsTooHigh.selector);
        executor.executeArb(steps, address(token), 1000, 10001);
    }

    function test_tipBps_boundary_10000_accepted() public {
        // tipBps = 10000 (100%) should NOT revert with TipBpsTooHigh
        // Verified via the full-flow test_executeArb_tipBps10000_allProfitToCoinbase
        // Here we just confirm 10001 reverts and 10000 does not trigger TipBpsTooHigh
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.expectRevert(AetherExecutor.TipBpsTooHigh.selector);
        executor.executeArb(steps, address(token), 1000, 10001);
        // 10000 does NOT revert with TipBpsTooHigh (call proceeds past the check)
        // The low-level call to the mock AAVE_POOL (an EOA) succeeds silently,
        // which is fine for this boundary check — the full flow is tested elsewhere
        executor.executeArb(steps, address(token), 1000, 10000);
    }

    function testFuzz_tipBps_tooHigh(uint256 tipBps) public {
        vm.assume(tipBps > 10000);
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.expectRevert(AetherExecutor.TipBpsTooHigh.selector);
        executor.executeArb(steps, address(token), 1000, tipBps);
    }

    function test_executeArb_inlineTip() public {
        // Deploy mock Aave pool and create executor bound to it
        MockAavePool mockPool = new MockAavePool();
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool));

        // Deploy two mock tokens (same token used for in/out to keep it simple)
        MockERC20 arbToken = new MockERC20();

        // Flash loan: borrow 100_000 tokens
        // Premium (0.05%): 50 tokens
        // Total debt: 100_050
        uint256 flashloanAmount = 100_000;
        uint256 premium = (flashloanAmount * 5) / 10000; // 50
        uint256 totalDebt = flashloanAmount + premium;

        // The mock swap pool will return flashloanAmount + extra profit
        // We want 1000 tokens of profit after repaying debt
        uint256 targetProfit = 1000;
        uint256 swapOut = totalDebt + targetProfit; // 101_050

        MockSwapPool swapPool = new MockSwapPool(address(arbToken), swapOut);

        // Build a single swap step (UniswapV2 protocol=1)
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](1);
        steps[0] = AetherExecutor.SwapStep({
            protocol: 1, // UNISWAP_V2
            pool: address(swapPool),
            tokenIn: address(arbToken),
            tokenOut: address(arbToken),
            amountIn: flashloanAmount,
            minAmountOut: swapOut,
            data: abi.encodeWithSignature("swap()") // triggers fallback
        });

        // Set block.coinbase so we can verify tip recipient
        address coinbase = address(0xC01B);
        vm.coinbase(coinbase);

        // tipBps = 9000 (90%)
        uint256 tipBps = 9000;
        uint256 expectedTip = (targetProfit * tipBps) / 10000; // 900
        uint256 expectedOwner = targetProfit - expectedTip; // 100

        // Expect the ArbExecuted event: check indexed topic (flashloanToken)
        // but skip non-indexed data check since gasUsed is non-deterministic
        vm.expectEmit(true, false, false, false);
        emit ArbExecuted(
            address(arbToken),
            flashloanAmount,
            targetProfit,
            expectedTip,
            0 // gasUsed placeholder, not checked
        );

        // Execute
        tipExecutor.executeArb(steps, address(arbToken), flashloanAmount, tipBps);

        // Verify tip went to coinbase
        assertEq(arbToken.balanceOf(coinbase), expectedTip, "coinbase tip incorrect");
        // Verify remainder went to owner (this test contract is the owner)
        assertEq(arbToken.balanceOf(address(this)), expectedOwner, "owner profit incorrect");
        // Verify executor has no leftover
        assertEq(arbToken.balanceOf(address(tipExecutor)), 0, "executor should have zero balance");
    }

    function test_executeArb_tipBpsZero_allProfitToOwner() public {
        MockAavePool mockPool = new MockAavePool();
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool));
        MockERC20 arbToken = new MockERC20();

        uint256 flashloanAmount = 100_000;
        uint256 premium = (flashloanAmount * 5) / 10000;
        uint256 totalDebt = flashloanAmount + premium;
        uint256 targetProfit = 1000;
        uint256 swapOut = totalDebt + targetProfit;

        MockSwapPool swapPool = new MockSwapPool(address(arbToken), swapOut);

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](1);
        steps[0] = AetherExecutor.SwapStep({
            protocol: 1,
            pool: address(swapPool),
            tokenIn: address(arbToken),
            tokenOut: address(arbToken),
            amountIn: flashloanAmount,
            minAmountOut: swapOut,
            data: abi.encodeWithSignature("swap()")
        });

        address coinbase = address(0xC01B);
        vm.coinbase(coinbase);

        // tipBps = 0: all profit goes to owner
        tipExecutor.executeArb(steps, address(arbToken), flashloanAmount, 0);

        assertEq(arbToken.balanceOf(coinbase), 0, "coinbase should get nothing");
        assertEq(arbToken.balanceOf(address(this)), targetProfit, "owner should get all profit");
        assertEq(arbToken.balanceOf(address(tipExecutor)), 0, "executor should have zero balance");
    }

    function test_executeArb_tipBps10000_allProfitToCoinbase() public {
        MockAavePool mockPool = new MockAavePool();
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool));
        MockERC20 arbToken = new MockERC20();

        uint256 flashloanAmount = 100_000;
        uint256 premium = (flashloanAmount * 5) / 10000;
        uint256 totalDebt = flashloanAmount + premium;
        uint256 targetProfit = 1000;
        uint256 swapOut = totalDebt + targetProfit;

        MockSwapPool swapPool = new MockSwapPool(address(arbToken), swapOut);

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](1);
        steps[0] = AetherExecutor.SwapStep({
            protocol: 1,
            pool: address(swapPool),
            tokenIn: address(arbToken),
            tokenOut: address(arbToken),
            amountIn: flashloanAmount,
            minAmountOut: swapOut,
            data: abi.encodeWithSignature("swap()")
        });

        address coinbase = address(0xC01B);
        vm.coinbase(coinbase);

        // tipBps = 10000: all profit goes to coinbase
        tipExecutor.executeArb(steps, address(arbToken), flashloanAmount, 10000);

        assertEq(arbToken.balanceOf(coinbase), targetProfit, "coinbase should get all profit");
        assertEq(arbToken.balanceOf(address(this)), 0, "owner should get nothing");
        assertEq(arbToken.balanceOf(address(tipExecutor)), 0, "executor should have zero balance");
    }

    function testFuzz_tipBps_profitSplit(uint256 tipBps) public {
        vm.assume(tipBps <= 10000);

        (AetherExecutor tipExecutor, MockERC20 arbToken) = _deployArbFixture(10_000);

        address coinbase = address(0xC01B);
        vm.coinbase(coinbase);

        tipExecutor.executeArb(_buildSingleStep(arbToken, 10_000), address(arbToken), 100_000, tipBps);

        uint256 expectedTip = (10_000 * tipBps) / 10000;
        assertEq(arbToken.balanceOf(coinbase), expectedTip, "coinbase tip incorrect");
        assertEq(arbToken.balanceOf(address(this)), 10_000 - expectedTip, "owner profit incorrect");
        // No tokens lost
        assertEq(
            arbToken.balanceOf(coinbase) + arbToken.balanceOf(address(this)),
            10_000,
            "total distributed must equal profit"
        );
    }

    // --- Helpers ---

    /// @dev Deploy a mock Aave pool + executor + token, with a swap pool that yields targetProfit
    function _deployArbFixture(uint256 targetProfit)
        internal
        returns (AetherExecutor tipExecutor, MockERC20 arbToken)
    {
        MockAavePool mockPool = new MockAavePool();
        tipExecutor = new AetherExecutor(address(mockPool));
        arbToken = new MockERC20();
        // Swap pool output = totalDebt + targetProfit
        uint256 swapOut = 100_000 + (100_000 * 5) / 10000 + targetProfit;
        MockSwapPool swapPool = new MockSwapPool(address(arbToken), swapOut);
        // Store swap pool address for step building
        _lastSwapPool = address(swapPool);
    }

    address private _lastSwapPool;

    /// @dev Build a single UniV2 swap step using the last deployed swap pool
    function _buildSingleStep(MockERC20 arbToken, uint256 targetProfit)
        internal
        view
        returns (AetherExecutor.SwapStep[] memory steps)
    {
        uint256 swapOut = 100_000 + (100_000 * 5) / 10000 + targetProfit;
        steps = new AetherExecutor.SwapStep[](1);
        steps[0] = AetherExecutor.SwapStep({
            protocol: 1,
            pool: _lastSwapPool,
            tokenIn: address(arbToken),
            tokenOut: address(arbToken),
            amountIn: 100_000,
            minAmountOut: swapOut,
            data: abi.encodeWithSignature("swap()")
        });
    }
}
