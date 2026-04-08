// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {AetherExecutor} from "../src/AetherExecutor.sol";

/// @dev Mock Aave pool that reverts on flashLoanSimple (used to test FlashLoanFailed)
contract RevertingAavePool {
    fallback() external {
        revert("pool reverted");
    }
}

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

/// @dev Mock WETH that supports deposit/withdraw with native ETH
contract MockWETH {
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

    /// @dev Simulates WETH.withdraw: burns WETH balance and sends native ETH
    function withdraw(uint256 wad) external {
        require(balanceOf[msg.sender] >= wad, "Insufficient WETH balance");
        balanceOf[msg.sender] -= wad;
        (bool sent,) = msg.sender.call{value: wad}("");
        require(sent, "ETH transfer failed");
    }

    receive() external payable {}
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
    MockERC20 token2;
    address constant AAVE_POOL = address(0xAA);
    address owner;

    function setUp() public {
        owner = address(this);
        executor = new AetherExecutor(AAVE_POOL);
        token = new MockERC20();
        token2 = new MockERC20();
    }

    // -------------------------------------------------------------------------
    // Basic state
    // -------------------------------------------------------------------------

    function test_owner() public view {
        assertEq(executor.owner(), owner);
    }

    function test_aavePool() public view {
        assertEq(executor.aavePool(), AAVE_POOL);
    }

    // -------------------------------------------------------------------------
    // transferOwnership
    // -------------------------------------------------------------------------

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
        vm.expectRevert(AetherExecutor.ZeroAddress.selector);
        executor.transferOwnership(address(0));
    }

    // -------------------------------------------------------------------------
    // rescue
    // -------------------------------------------------------------------------

    function test_rescue() public {
        token.mint(address(executor), 1000);
        assertEq(token.balanceOf(address(executor)), 1000);

        executor.rescue(address(token), 1000);
        assertEq(token.balanceOf(owner), 1000);
        assertEq(token.balanceOf(address(executor)), 0);
    }

    function test_rescue_revert_notOwner() public {
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.rescue(address(token), 100);
    }

    // -------------------------------------------------------------------------
    // executeArb - access control
    // -------------------------------------------------------------------------

    function test_executeArb_revert_notOwner() public {
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.executeArb(steps, address(token), 1000, 9000);
    }

    // -------------------------------------------------------------------------
    // executeArb - FlashLoanFailed when pool call reverts
    // -------------------------------------------------------------------------

    function test_executeArb_revert_flashLoanFailed() public {
        // Deploy an executor backed by a pool that always reverts
        RevertingAavePool badPool = new RevertingAavePool();
        AetherExecutor executorWithBadPool = new AetherExecutor(address(badPool));

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.expectRevert(AetherExecutor.FlashLoanFailed.selector);
        executorWithBadPool.executeArb(steps, address(token), 1000, 9000);
    }

    // -------------------------------------------------------------------------
    // executeOperation - access control
    // -------------------------------------------------------------------------

    function test_executeOperation_revert_notAavePool() public {
        // Calling from an address that is not the Aave pool must revert
        vm.prank(address(0xBB));
        vm.expectRevert(AetherExecutor.NotAavePool.selector);
        executor.executeOperation(address(token), 1000, 5, address(executor), "");
    }

    function test_executeOperation_revert_invalidInitiator() public {
        // Called from the correct Aave pool address but with a foreign initiator
        vm.prank(AAVE_POOL);
        vm.expectRevert(AetherExecutor.InvalidInitiator.selector);
        executor.executeOperation(address(token), 1000, 5, address(0xDEAD), "");
    }

    // -------------------------------------------------------------------------
    // setApprovals
    // -------------------------------------------------------------------------

    function test_setApprovals() public {
        address[] memory tokens = new address[](1);
        address[] memory spenders = new address[](1);
        tokens[0] = address(token);
        spenders[0] = AAVE_POOL;

        executor.setApprovals(tokens, spenders);

        assertEq(token.allowance(address(executor), AAVE_POOL), type(uint256).max);
    }

    function test_setApprovals_multiple() public {
        address[] memory tokens = new address[](2);
        address[] memory spenders = new address[](2);
        tokens[0] = address(token);
        tokens[1] = address(token2);
        spenders[0] = AAVE_POOL;
        spenders[1] = address(0xBB);

        executor.setApprovals(tokens, spenders);

        assertEq(token.allowance(address(executor), AAVE_POOL), type(uint256).max);
        assertEq(token2.allowance(address(executor), address(0xBB)), type(uint256).max);
    }

    function test_setApprovals_revert_notOwner() public {
        address[] memory tokens = new address[](1);
        address[] memory spenders = new address[](1);
        tokens[0] = address(token);
        spenders[0] = AAVE_POOL;

        vm.prank(address(0x456));
        vm.expectRevert(AetherExecutor.NotOwner.selector);
        executor.setApprovals(tokens, spenders);
    }

    // -------------------------------------------------------------------------
    // ETH receive
    // -------------------------------------------------------------------------

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

    // --- WETH tip tests ---

    function test_executeArb_wethTip_sendsNativeEth() public {
        address WETH_ADDR = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

        // Deploy MockWETH code at the canonical WETH address
        _deployMockWethAt(WETH_ADDR);

        (AetherExecutor wethExecutor, AetherExecutor.SwapStep[] memory steps) =
            _buildWethArbFixture(WETH_ADDR, 1000);

        address coinbase = address(0xC01B);
        vm.coinbase(coinbase);

        // Fund MockWETH with native ETH so withdraw() can send ETH back
        vm.deal(WETH_ADDR, 10_000);

        // tipBps=9000 -> tip=900, ownerProfit=100
        wethExecutor.executeArb(steps, WETH_ADDR, 100_000, 9000);

        // Coinbase received native ETH, not WETH tokens
        assertEq(coinbase.balance, 900, "coinbase should receive native ETH tip");
        assertEq(MockWETH(payable(WETH_ADDR)).balanceOf(coinbase), 0, "coinbase should not hold WETH");
        // Owner still receives WETH (not unwrapped)
        assertEq(MockWETH(payable(WETH_ADDR)).balanceOf(address(this)), 100, "owner WETH profit incorrect");
        // Executor has no leftover
        assertEq(MockWETH(payable(WETH_ADDR)).balanceOf(address(wethExecutor)), 0, "executor should have zero WETH");
    }

    function test_executeArb_nonWeth_sendsErc20Tip() public {
        // Verify that non-WETH assets still use ERC-20 transfer (no native ETH sent)
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

        uint256 tipBps = 9000;
        uint256 expectedTip = (targetProfit * tipBps) / 10000;

        tipExecutor.executeArb(steps, address(arbToken), flashloanAmount, tipBps);

        // Coinbase received ERC-20, not native ETH
        assertEq(arbToken.balanceOf(coinbase), expectedTip, "coinbase should receive ERC-20 tip");
        assertEq(coinbase.balance, 0, "coinbase should not receive native ETH for non-WETH");
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

    /// @dev Deploy MockWETH bytecode at a specific address using vm.etch
    function _deployMockWethAt(address target) internal {
        bytes memory wethCode = type(MockWETH).creationCode;
        address deployed;
        assembly { deployed := create(0, add(wethCode, 0x20), mload(wethCode)) }
        vm.etch(target, deployed.code);
    }

    /// @dev Build executor + swap steps for a WETH arb with given profit
    function _buildWethArbFixture(address wethAddr, uint256 targetProfit)
        internal
        returns (AetherExecutor wethExecutor, AetherExecutor.SwapStep[] memory steps)
    {
        MockAavePool mockPool = new MockAavePool();
        wethExecutor = new AetherExecutor(address(mockPool));

        uint256 swapOut = 100_000 + (100_000 * 5) / 10000 + targetProfit;
        MockSwapPool swapPool = new MockSwapPool(wethAddr, swapOut);

        steps = new AetherExecutor.SwapStep[](1);
        steps[0] = AetherExecutor.SwapStep({
            protocol: 1,
            pool: address(swapPool),
            tokenIn: wethAddr,
            tokenOut: wethAddr,
            amountIn: 100_000,
            minAmountOut: swapOut,
            data: abi.encodeWithSignature("swap()")
        });
    }

    // -------------------------------------------------------------------------
    // setApprovals - input validation
    // -------------------------------------------------------------------------

    function test_setApprovals_revert_arrayLengthMismatch() public {
        address[] memory tokens = new address[](2);
        address[] memory spenders = new address[](1);
        tokens[0] = address(token);
        tokens[1] = address(token2);
        spenders[0] = AAVE_POOL;

        vm.expectRevert(AetherExecutor.ArrayLengthMismatch.selector);
        executor.setApprovals(tokens, spenders);
    }

    function test_setApprovals_revert_zeroAddressSpender() public {
        address[] memory tokens = new address[](1);
        address[] memory spenders = new address[](1);
        tokens[0] = address(token);
        spenders[0] = address(0);

        vm.expectRevert(AetherExecutor.ZeroAddress.selector);
        executor.setApprovals(tokens, spenders);
    }

    // -------------------------------------------------------------------------
    // Fuzz: setApprovals empty array is a no-op (no revert)
    // -------------------------------------------------------------------------

    function testFuzz_setApprovals_emptyArrays() public {
        address[] memory tokens = new address[](0);
        address[] memory spenders = new address[](0);
        // Should not revert
        executor.setApprovals(tokens, spenders);
    }
}
