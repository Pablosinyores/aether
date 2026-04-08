// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
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

/// @dev Mock UniV2 pair — expects tokens pre-transferred, calls swap to send output
contract MockV2Pool {
    MockERC20 public immutable tokenIn;
    MockERC20 public immutable tokenOut;
    uint256 public immutable amountOut;

    constructor(MockERC20 _tokenIn, MockERC20 _tokenOut, uint256 _amountOut) {
        tokenIn = _tokenIn;
        tokenOut = _tokenOut;
        amountOut = _amountOut;
    }

    /// @dev UniswapV2Pair.swap — tokens already transferred in, just send output
    fallback() external {
        tokenOut.transfer(msg.sender, amountOut);
    }
}

/// @dev Mock UniV3 pool — calls uniswapV3SwapCallback on msg.sender to pull tokenIn
contract MockV3Pool {
    MockERC20 public immutable tokenIn;
    MockERC20 public immutable tokenOut;
    uint256 public immutable amountIn;
    uint256 public immutable amountOut;

    constructor(MockERC20 _tokenIn, MockERC20 _tokenOut, uint256 _amountIn, uint256 _amountOut) {
        tokenIn = _tokenIn;
        tokenOut = _tokenOut;
        amountIn = _amountIn;
        amountOut = _amountOut;
    }

    /// @dev On any call, invoke uniswapV3SwapCallback then send output tokens
    fallback() external {
        bytes memory callbackData = abi.encodeWithSignature(
            "uniswapV3SwapCallback(int256,int256,bytes)",
            int256(amountIn),
            int256(0),
            ""
        );
        (bool success,) = msg.sender.call(callbackData);
        require(success, "V3 callback failed");

        require(tokenIn.balanceOf(address(this)) >= amountIn, "V3: tokens not received");

        tokenOut.transfer(msg.sender, amountOut);
    }
}

/// @dev Malicious V3 pool that calls uniswapV3SwapCallback twice to attempt double-spend
contract MockMaliciousV3Pool {
    MockERC20 public immutable tokenIn;
    MockERC20 public immutable tokenOut;
    uint256 public immutable amountIn;
    uint256 public immutable amountOut;

    constructor(MockERC20 _tokenIn, MockERC20 _tokenOut, uint256 _amountIn, uint256 _amountOut) {
        tokenIn = _tokenIn;
        tokenOut = _tokenOut;
        amountIn = _amountIn;
        amountOut = _amountOut;
    }

    fallback() external {
        // First callback — should transfer tokens
        bytes memory callbackData = abi.encodeWithSignature(
            "uniswapV3SwapCallback(int256,int256,bytes)",
            int256(amountIn),
            int256(0),
            ""
        );
        (bool success1,) = msg.sender.call(callbackData);
        require(success1, "First callback failed");

        // Second callback — should transfer 0 (amountIn already zeroed)
        (bool success2,) = msg.sender.call(callbackData);
        require(success2, "Second callback failed");

        // Send output tokens
        tokenOut.transfer(msg.sender, amountOut);
    }
}

/// @dev Mock Curve pool — pulls tokenIn via transferFrom, sends tokenOut
contract MockCurvePool {
    MockERC20 public immutable tokenIn;
    MockERC20 public immutable tokenOut;
    uint256 public immutable amountOut;

    constructor(MockERC20 _tokenIn, MockERC20 _tokenOut, uint256 _amountOut) {
        tokenIn = _tokenIn;
        tokenOut = _tokenOut;
        amountOut = _amountOut;
    }

    fallback() external {
        uint256 approved = tokenIn.allowance(msg.sender, address(this));
        require(approved > 0, "Curve: no approval");
        tokenIn.transferFrom(msg.sender, address(this), approved);
        tokenOut.transfer(msg.sender, amountOut);
    }
}

/// @dev Mock Balancer Vault — pulls tokenIn via transferFrom, sends tokenOut
contract MockBalancerVault {
    MockERC20 public immutable tokenIn;
    MockERC20 public immutable tokenOut;
    uint256 public immutable amountOut;

    constructor(MockERC20 _tokenIn, MockERC20 _tokenOut, uint256 _amountOut) {
        tokenIn = _tokenIn;
        tokenOut = _tokenOut;
        amountOut = _amountOut;
    }

    fallback() external {
        uint256 approved = tokenIn.allowance(msg.sender, address(this));
        require(approved > 0, "Balancer: no approval");
        tokenIn.transferFrom(msg.sender, address(this), approved);
        tokenOut.transfer(msg.sender, amountOut);
    }
}

/// @dev Mock Bancor router — pulls tokenIn via transferFrom, sends tokenOut
contract MockBancorRouter {
    MockERC20 public immutable tokenIn;
    MockERC20 public immutable tokenOut;
    uint256 public immutable amountOut;

    constructor(MockERC20 _tokenIn, MockERC20 _tokenOut, uint256 _amountOut) {
        tokenIn = _tokenIn;
        tokenOut = _tokenOut;
        amountOut = _amountOut;
    }

    fallback() external {
        uint256 approved = tokenIn.allowance(msg.sender, address(this));
        require(approved > 0, "Bancor: no approval");
        tokenIn.transferFrom(msg.sender, address(this), approved);
        tokenOut.transfer(msg.sender, amountOut);
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
    MockAavePool aavePool;
    address owner;

    // Protocol constants (must match contract)
    uint8 constant UNISWAP_V2 = 1;
    uint8 constant UNISWAP_V3 = 2;
    uint8 constant SUSHISWAP = 3;
    uint8 constant CURVE = 4;
    uint8 constant BALANCER_V2 = 5;
    uint8 constant BANCOR_V3 = 6;

    function setUp() public {
        owner = address(this);
        aavePool = new MockAavePool();
        executor = new AetherExecutor(address(aavePool), address(0xBA12));
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
        assertEq(executor.aavePool(), address(aavePool));
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
        AetherExecutor executorWithBadPool = new AetherExecutor(address(badPool), address(0xBA12));

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
        vm.prank(address(aavePool));
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
        spenders[0] = address(aavePool);

        executor.setApprovals(tokens, spenders);

        assertEq(token.allowance(address(executor), address(aavePool)), type(uint256).max);
    }

    function test_setApprovals_multiple() public {
        address[] memory tokens = new address[](2);
        address[] memory spenders = new address[](2);
        tokens[0] = address(token);
        tokens[1] = address(token2);
        spenders[0] = address(aavePool);
        spenders[1] = address(0xBB);

        executor.setApprovals(tokens, spenders);

        assertEq(token.allowance(address(executor), address(aavePool)), type(uint256).max);
        assertEq(token2.allowance(address(executor), address(0xBB)), type(uint256).max);
    }

    function test_setApprovals_revert_notOwner() public {
        address[] memory tokens = new address[](1);
        address[] memory spenders = new address[](1);
        tokens[0] = address(token);
        spenders[0] = address(aavePool);

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
        // Use an EOA-backed executor so the flashLoan call succeeds silently
        AetherExecutor eoaExecutor = new AetherExecutor(address(0xAA), address(0xBA12));
        eoaExecutor.executeArb(steps, address(token), 1000, 10000);
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
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12));

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
            minAmountOut: 1,
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
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12));
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
            minAmountOut: 1,
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
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12));
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
            minAmountOut: 1,
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
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12));
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
            minAmountOut: 1,
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
        tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12));
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
            minAmountOut: 1,
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
        wethExecutor = new AetherExecutor(address(mockPool), address(0xBA12));

        uint256 swapOut = 100_000 + (100_000 * 5) / 10000 + targetProfit;
        MockSwapPool swapPool = new MockSwapPool(wethAddr, swapOut);

        steps = new AetherExecutor.SwapStep[](1);
        steps[0] = AetherExecutor.SwapStep({
            protocol: 1,
            pool: address(swapPool),
            tokenIn: wethAddr,
            tokenOut: wethAddr,
            amountIn: 100_000,
            minAmountOut: 1,
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
        spenders[0] = address(aavePool);

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

    // -------------------------------------------------------------------------
    // UniV2/Sushi swap tests (pre-transfer pattern)
    // -------------------------------------------------------------------------

    function test_swapUniV2_preTransferPattern() public {
        MockERC20 tokenIn = new MockERC20();
        MockERC20 tokenOut = new MockERC20();

        uint256 flashAmount = 1000;
        uint256 swapOut = 1100;
        uint256 premium = flashAmount * 5 / 10000; // 0.05%

        // Create mock V2 pool with sufficient output tokens
        MockV2Pool pool = new MockV2Pool(tokenIn, tokenOut, swapOut);
        tokenOut.mint(address(pool), swapOut);

        // Build swap step — V2 swap(uint,uint,address,bytes)
        bytes memory swapData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0),
            swapOut,
            address(executor),
            ""
        );

        // Build a two-step arb: tokenIn -> tokenOut (V2), tokenOut -> tokenIn (V2 again for repay)
        // Simpler: single-step with flash loan in tokenOut (so profit is in tokenOut after step)
        // Simplest: use tokenIn as the flash loan token
        // Step 1: swap tokenIn -> tokenOut via V2

        // We need the executor to end with more tokenIn than it borrowed
        // So: flash borrow tokenIn, swap to tokenOut, swap back to tokenIn with profit

        // For simplicity, let's do a single-step with a second pool for the return
        uint256 returnAmount = flashAmount + premium + 10; // enough to repay + profit
        MockV2Pool returnPool = new MockV2Pool(tokenOut, tokenIn, returnAmount);
        tokenIn.mint(address(returnPool), returnAmount);

        bytes memory returnData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0),
            returnAmount,
            address(executor),
            ""
        );

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](2);
        steps[0] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(pool),
            tokenIn: address(tokenIn),
            tokenOut: address(tokenOut),
            amountIn: flashAmount,
            minAmountOut: swapOut,
            data: swapData
        });
        steps[1] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(returnPool),
            tokenIn: address(tokenOut),
            tokenOut: address(tokenIn),
            amountIn: swapOut,
            minAmountOut: returnAmount,
            data: returnData
        });

        // Verify: pool should receive tokens via transfer (not transferFrom)
        // Before the swap, pool has 0 tokenIn; after, it has flashAmount
        executor.executeArb(steps, address(tokenIn), flashAmount, 0);

        // Check pool received tokenIn via direct transfer
        assertEq(tokenIn.balanceOf(address(pool)), flashAmount);
        // Check owner received profit
        assertGt(tokenIn.balanceOf(owner), 0);
    }

    // -------------------------------------------------------------------------
    // UniV3 swap tests (callback pattern)
    // -------------------------------------------------------------------------

    function test_swapUniV3_callbackPattern() public {
        MockERC20 tokenIn = new MockERC20();
        MockERC20 tokenOut = new MockERC20();

        uint256 flashAmount = 1000;
        uint256 swapOut = 1100;
        uint256 premium = flashAmount * 5 / 10000;

        // Create V3 pool
        MockV3Pool v3Pool = new MockV3Pool(tokenIn, tokenOut, flashAmount, swapOut);
        tokenOut.mint(address(v3Pool), swapOut);

        // Create return pool (V2 to simplify — swap tokenOut back to tokenIn)
        uint256 returnAmount = flashAmount + premium + 10;
        MockV2Pool returnPool = new MockV2Pool(tokenOut, tokenIn, returnAmount);
        tokenIn.mint(address(returnPool), returnAmount);

        // V3 swap calldata (arbitrary — mock uses fallback)
        bytes memory v3SwapData = abi.encodeWithSignature(
            "swap(address,bool,int256,uint160,bytes)",
            address(executor),
            true,
            int256(flashAmount),
            uint160(0),
            ""
        );

        bytes memory returnData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0),
            returnAmount,
            address(executor),
            ""
        );

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](2);
        steps[0] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V3,
            pool: address(v3Pool),
            tokenIn: address(tokenIn),
            tokenOut: address(tokenOut),
            amountIn: flashAmount,
            minAmountOut: swapOut,
            data: v3SwapData
        });
        steps[1] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(returnPool),
            tokenIn: address(tokenOut),
            tokenOut: address(tokenIn),
            amountIn: swapOut,
            minAmountOut: returnAmount,
            data: returnData
        });

        executor.executeArb(steps, address(tokenIn), flashAmount, 0);

        // V3 pool received tokens via callback (not pre-transfer)
        assertEq(tokenIn.balanceOf(address(v3Pool)), flashAmount);
        // Owner received profit
        assertGt(tokenIn.balanceOf(owner), 0);
    }

    function test_uniV3Callback_revert_notPendingPool() public {
        // Calling uniswapV3SwapCallback from a non-pool address should revert
        vm.prank(address(0xDEAD));
        vm.expectRevert(AetherExecutor.NotPendingV3Pool.selector);
        executor.uniswapV3SwapCallback(int256(100), int256(0), "");
    }

    function test_uniV3Callback_doubleCall_onlyFirstTransfers() public {
        MockERC20 tokenIn = new MockERC20();
        MockERC20 tokenOut = new MockERC20();

        uint256 flashAmount = 1000;
        uint256 swapOut = 1100;
        uint256 premium = flashAmount * 5 / 10000;

        // Malicious pool that calls callback twice
        MockMaliciousV3Pool malPool = new MockMaliciousV3Pool(tokenIn, tokenOut, flashAmount, swapOut);
        tokenOut.mint(address(malPool), swapOut);

        // Return pool
        uint256 returnAmount = flashAmount + premium + 10;
        MockV2Pool returnPool = new MockV2Pool(tokenOut, tokenIn, returnAmount);
        tokenIn.mint(address(returnPool), returnAmount);

        bytes memory v3SwapData = abi.encodeWithSignature(
            "swap(address,bool,int256,uint160,bytes)",
            address(executor), true, int256(flashAmount), uint160(0), ""
        );
        bytes memory returnData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0), returnAmount, address(executor), ""
        );

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](2);
        steps[0] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V3,
            pool: address(malPool),
            tokenIn: address(tokenIn),
            tokenOut: address(tokenOut),
            amountIn: flashAmount,
            minAmountOut: swapOut,
            data: v3SwapData
        });
        steps[1] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(returnPool),
            tokenIn: address(tokenOut),
            tokenOut: address(tokenIn),
            amountIn: swapOut,
            minAmountOut: returnAmount,
            data: returnData
        });

        executor.executeArb(steps, address(tokenIn), flashAmount, 0);

        // Malicious pool only received flashAmount (not 2x) despite calling back twice
        assertEq(tokenIn.balanceOf(address(malPool)), flashAmount, "double-call should not drain extra tokens");
        assertGt(tokenIn.balanceOf(owner), 0, "owner should still receive profit");
    }

    // -------------------------------------------------------------------------
    // Curve swap tests (approve + pull pattern)
    // -------------------------------------------------------------------------

    function test_swapCurve_pullPattern() public {
        MockERC20 tokenIn = new MockERC20();
        MockERC20 tokenOut = new MockERC20();

        uint256 flashAmount = 1000;
        uint256 swapOut = 1100;
        uint256 premium = flashAmount * 5 / 10000;

        // Create Curve pool
        MockCurvePool curvePool = new MockCurvePool(tokenIn, tokenOut, swapOut);
        tokenOut.mint(address(curvePool), swapOut);

        // Return pool
        uint256 returnAmount = flashAmount + premium + 10;
        MockV2Pool returnPool = new MockV2Pool(tokenOut, tokenIn, returnAmount);
        tokenIn.mint(address(returnPool), returnAmount);

        bytes memory curveData = abi.encodeWithSignature(
            "exchange(int128,int128,uint256,uint256)",
            int128(0),
            int128(1),
            flashAmount,
            swapOut
        );
        bytes memory returnData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0),
            returnAmount,
            address(executor),
            ""
        );

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](2);
        steps[0] = AetherExecutor.SwapStep({
            protocol: CURVE,
            pool: address(curvePool),
            tokenIn: address(tokenIn),
            tokenOut: address(tokenOut),
            amountIn: flashAmount,
            minAmountOut: swapOut,
            data: curveData
        });
        steps[1] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(returnPool),
            tokenIn: address(tokenOut),
            tokenOut: address(tokenIn),
            amountIn: swapOut,
            minAmountOut: returnAmount,
            data: returnData
        });

        executor.executeArb(steps, address(tokenIn), flashAmount, 0);

        // Curve pool pulled tokens via transferFrom (approve+pull pattern)
        assertEq(tokenIn.balanceOf(address(curvePool)), flashAmount);
        // Approval should be reset to 0 after swap
        assertEq(tokenIn.allowance(address(executor), address(curvePool)), 0);
        // Owner received profit
        assertGt(tokenIn.balanceOf(owner), 0);
    }

    // -------------------------------------------------------------------------
    // Balancer swap tests (Vault approve + pull pattern)
    // -------------------------------------------------------------------------

    function test_swapBalancer_vaultPattern() public {
        MockERC20 tokenIn = new MockERC20();
        MockERC20 tokenOut = new MockERC20();

        uint256 flashAmount = 1000;
        uint256 swapOut = 1100;
        uint256 premium = flashAmount * 5 / 10000;

        // Create Balancer Vault and deploy executor pointing at it
        MockBalancerVault vault = new MockBalancerVault(tokenIn, tokenOut, swapOut);
        tokenOut.mint(address(vault), swapOut);
        AetherExecutor balExecutor = new AetherExecutor(address(aavePool), address(vault));

        // Return pool
        uint256 returnAmount = flashAmount + premium + 10;
        MockV2Pool returnPool = new MockV2Pool(tokenOut, tokenIn, returnAmount);
        tokenIn.mint(address(returnPool), returnAmount);

        bytes memory balancerData = abi.encodeWithSignature(
            "swap(bytes32,address,address,uint256,uint256)",
            bytes32(0),
            address(tokenIn),
            address(tokenOut),
            flashAmount,
            swapOut
        );
        bytes memory returnData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0),
            returnAmount,
            address(balExecutor),
            ""
        );

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](2);
        steps[0] = AetherExecutor.SwapStep({
            protocol: BALANCER_V2,
            pool: address(vault),
            tokenIn: address(tokenIn),
            tokenOut: address(tokenOut),
            amountIn: flashAmount,
            minAmountOut: swapOut,
            data: balancerData
        });
        steps[1] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(returnPool),
            tokenIn: address(tokenOut),
            tokenOut: address(tokenIn),
            amountIn: swapOut,
            minAmountOut: returnAmount,
            data: returnData
        });

        balExecutor.executeArb(steps, address(tokenIn), flashAmount, 0);

        // Vault pulled tokens via transferFrom (now through balancerVault immutable)
        assertEq(tokenIn.balanceOf(address(vault)), flashAmount);
        // Approval to vault reset to 0
        assertEq(tokenIn.allowance(address(balExecutor), address(vault)), 0);
        // Owner received profit
        assertGt(tokenIn.balanceOf(owner), 0);
    }

    // -------------------------------------------------------------------------
    // Bancor swap tests (approve + pull pattern)
    // -------------------------------------------------------------------------

    function test_swapBancor_pullPattern() public {
        MockERC20 tokenIn = new MockERC20();
        MockERC20 tokenOut = new MockERC20();

        uint256 flashAmount = 1000;
        uint256 swapOut = 1100;
        uint256 premium = flashAmount * 5 / 10000;

        // Create Bancor router
        MockBancorRouter bancor = new MockBancorRouter(tokenIn, tokenOut, swapOut);
        tokenOut.mint(address(bancor), swapOut);

        // Return pool
        uint256 returnAmount = flashAmount + premium + 10;
        MockV2Pool returnPool = new MockV2Pool(tokenOut, tokenIn, returnAmount);
        tokenIn.mint(address(returnPool), returnAmount);

        bytes memory bancorData = abi.encodeWithSignature(
            "tradeBySourceAmount(address,address,uint256,uint256,uint256,address)",
            address(tokenIn),
            address(tokenOut),
            flashAmount,
            swapOut,
            uint256(block.timestamp + 3600),
            address(executor)
        );
        bytes memory returnData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0),
            returnAmount,
            address(executor),
            ""
        );

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](2);
        steps[0] = AetherExecutor.SwapStep({
            protocol: BANCOR_V3,
            pool: address(bancor),
            tokenIn: address(tokenIn),
            tokenOut: address(tokenOut),
            amountIn: flashAmount,
            minAmountOut: swapOut,
            data: bancorData
        });
        steps[1] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(returnPool),
            tokenIn: address(tokenOut),
            tokenOut: address(tokenIn),
            amountIn: swapOut,
            minAmountOut: returnAmount,
            data: returnData
        });

        executor.executeArb(steps, address(tokenIn), flashAmount, 0);

        // Bancor pulled tokens via transferFrom
        assertEq(tokenIn.balanceOf(address(bancor)), flashAmount);
        // Approval reset to 0
        assertEq(tokenIn.allowance(address(executor), address(bancor)), 0);
        // Owner received profit
        assertGt(tokenIn.balanceOf(owner), 0);
    }

    // -------------------------------------------------------------------------
    // SushiSwap test (same as UniV2 pre-transfer pattern)
    // -------------------------------------------------------------------------

    function test_swapSushi_preTransferPattern() public {
        MockERC20 tokenIn = new MockERC20();
        MockERC20 tokenOut = new MockERC20();

        uint256 flashAmount = 1000;
        uint256 swapOut = 1100;
        uint256 premium = flashAmount * 5 / 10000;

        MockV2Pool pool = new MockV2Pool(tokenIn, tokenOut, swapOut);
        tokenOut.mint(address(pool), swapOut);

        uint256 returnAmount = flashAmount + premium + 10;
        MockV2Pool returnPool = new MockV2Pool(tokenOut, tokenIn, returnAmount);
        tokenIn.mint(address(returnPool), returnAmount);

        bytes memory swapData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0), swapOut, address(executor), ""
        );
        bytes memory returnData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0), returnAmount, address(executor), ""
        );

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](2);
        steps[0] = AetherExecutor.SwapStep({
            protocol: SUSHISWAP,
            pool: address(pool),
            tokenIn: address(tokenIn),
            tokenOut: address(tokenOut),
            amountIn: flashAmount,
            minAmountOut: swapOut,
            data: swapData
        });
        steps[1] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(returnPool),
            tokenIn: address(tokenOut),
            tokenOut: address(tokenIn),
            amountIn: swapOut,
            minAmountOut: returnAmount,
            data: returnData
        });

        executor.executeArb(steps, address(tokenIn), flashAmount, 0);

        // Pool received tokens via direct transfer (same as UniV2)
        assertEq(tokenIn.balanceOf(address(pool)), flashAmount);
        assertGt(tokenIn.balanceOf(owner), 0);
    }
}
