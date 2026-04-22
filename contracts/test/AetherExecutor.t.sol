// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test, Vm} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
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

/// @dev Counting Balancer Vault — tracks swap call count to assert routing target
contract CountingBalancerVault {
    MockERC20 public immutable tokenIn;
    MockERC20 public immutable tokenOut;
    uint256 public immutable amountOut;
    uint256 public swapCallCount;

    constructor(MockERC20 _tokenIn, MockERC20 _tokenOut, uint256 _amountOut) {
        tokenIn = _tokenIn;
        tokenOut = _tokenOut;
        amountOut = _amountOut;
    }

    fallback() external {
        swapCallCount += 1;
        uint256 approved = tokenIn.allowance(msg.sender, address(this));
        require(approved > 0, "CountingVault: no approval");
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

    /// @dev Simulates WETH.deposit: mints WETH to sender equal to msg.value
    function deposit() external payable {
        balanceOf[msg.sender] += msg.value;
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

/// @dev Mock Aave pool that tracks flashLoanSimple call count so tests can prove
///      the pre-flashloan validation short-circuits before Aave is invoked.
contract CountingAavePool {
    uint256 public flashLoanCallCount;

    function flashLoanSimple(
        address receiver,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 /* referralCode */
    ) external {
        flashLoanCallCount += 1;

        MockERC20(asset).mint(receiver, amount);
        uint256 premium = (amount * 5) / 10000;

        AetherExecutor(payable(receiver)).executeOperation(
            asset,
            amount,
            premium,
            receiver,
            params
        );

        uint256 totalDebt = amount + premium;
        MockERC20(asset).transferFrom(receiver, address(this), totalDebt);
    }
}

/// @dev Helper whose receive() always reverts — simulates a contract-coinbase that
///      refuses plain ETH transfers (forces the WETH fallback path in _repayAndDistribute).
contract RevertingCoinbase {
    receive() external payable {
        revert("no eth");
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
        // address(0xBA12) = placeholder balancerVault, address(0xBAAC) = placeholder bancorNetwork
        executor = new AetherExecutor(address(aavePool), address(0xBA12), address(0xBAAC));
        token = new MockERC20();
        token2 = new MockERC20();
    }

    /// @dev Accept native ETH. Needed for test_rescue_eth where the owner (this contract)
    ///      receives native ETH via executor.rescue(address(0), amount).
    receive() external payable {}

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
    // transferOwnership (Ownable2Step — two-step handoff)
    // -------------------------------------------------------------------------

    function test_transferOwnership() public {
        // Ownable2Step: transferOwnership only nominates pendingOwner; the new owner
        // must call acceptOwnership() to complete the handoff.
        address newOwner = address(0x123);
        executor.transferOwnership(newOwner);

        // Owner unchanged after step 1
        assertEq(executor.owner(), owner);
        assertEq(executor.pendingOwner(), newOwner);

        // Step 2: new owner accepts
        vm.prank(newOwner);
        executor.acceptOwnership();

        assertEq(executor.owner(), newOwner);
        assertEq(executor.pendingOwner(), address(0));
    }

    function test_transferOwnership_revert_notOwner() public {
        vm.prank(address(0x456));
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, address(0x456))
        );
        executor.transferOwnership(address(0x789));
    }

    /// @dev Ownable2Step allows transferOwnership(address(0)) — it CANCELS a pending
    ///      transfer by clearing pendingOwner. It does NOT revert.
    function test_transferOwnership_cancel_withZeroAddress() public {
        // First nominate a new owner
        address pending = address(0x123);
        executor.transferOwnership(pending);
        assertEq(executor.pendingOwner(), pending);

        // Now cancel by passing address(0). This does NOT revert on Ownable2Step.
        executor.transferOwnership(address(0));
        assertEq(executor.pendingOwner(), address(0), "pendingOwner should be cleared");
        assertEq(executor.owner(), owner, "owner unchanged after cancel");
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
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, address(0x456))
        );
        executor.rescue(address(token), 100);
    }

    // -------------------------------------------------------------------------
    // executeArb - access control
    // -------------------------------------------------------------------------

    function test_executeArb_revert_notOwner() public {
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.prank(address(0x456));
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, address(0x456))
        );
        executor.executeArb(steps, address(token), 1000, block.timestamp + 1000, 0, 9000);
    }

    // -------------------------------------------------------------------------
    // executeArb - FlashLoanFailed when pool call reverts
    // -------------------------------------------------------------------------

    function test_executeArb_revert_flashLoanFailed() public {
        // Deploy an executor backed by a pool that always reverts
        RevertingAavePool badPool = new RevertingAavePool();
        AetherExecutor executorWithBadPool = new AetherExecutor(address(badPool), address(0xBA12), address(0xBAAC));

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.expectRevert(AetherExecutor.FlashLoanFailed.selector);
        executorWithBadPool.executeArb(steps, address(token), 1000, block.timestamp + 1000, 0, 9000);
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
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, address(0x456))
        );
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
        executor.executeArb(steps, address(token), 1000, block.timestamp + 1000, 0, 10001);
    }

    function test_tipBps_boundary_10000_accepted() public {
        // tipBps = 10000 (100%) should NOT revert with TipBpsTooHigh
        // Verified via the full-flow test_executeArb_tipBps10000_allProfitToCoinbase
        // Here we just confirm 10001 reverts and 10000 does not trigger TipBpsTooHigh
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.expectRevert(AetherExecutor.TipBpsTooHigh.selector);
        executor.executeArb(steps, address(token), 1000, block.timestamp + 1000, 0, 10001);
        // 10000 does NOT revert with TipBpsTooHigh (call proceeds past the check)
        // Use an EOA-backed executor so the flashLoan call succeeds silently
        AetherExecutor eoaExecutor = new AetherExecutor(address(0xAA), address(0xBA12), address(0xBAAC));
        eoaExecutor.executeArb(steps, address(token), 1000, block.timestamp + 1000, 0, 10000);
    }

    function testFuzz_tipBps_tooHigh(uint256 tipBps) public {
        vm.assume(tipBps > 10000);
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](0);
        vm.expectRevert(AetherExecutor.TipBpsTooHigh.selector);
        executor.executeArb(steps, address(token), 1000, block.timestamp + 1000, 0, tipBps);
    }

    function test_executeArb_inlineTip() public {
        // Deploy mock Aave pool and create executor bound to it
        MockAavePool mockPool = new MockAavePool();
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12), address(0xBAAC));

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
        tipExecutor.executeArb(steps, address(arbToken), flashloanAmount, block.timestamp + 1000, 0, tipBps);

        // Verify tip went to coinbase
        assertEq(arbToken.balanceOf(coinbase), expectedTip, "coinbase tip incorrect");
        // Verify remainder went to owner (this test contract is the owner)
        assertEq(arbToken.balanceOf(address(this)), expectedOwner, "owner profit incorrect");
        // Verify executor has no leftover
        assertEq(arbToken.balanceOf(address(tipExecutor)), 0, "executor should have zero balance");
    }

    function test_executeArb_tipBpsZero_allProfitToOwner() public {
        MockAavePool mockPool = new MockAavePool();
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12), address(0xBAAC));
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
        tipExecutor.executeArb(steps, address(arbToken), flashloanAmount, block.timestamp + 1000, 0, 0);

        assertEq(arbToken.balanceOf(coinbase), 0, "coinbase should get nothing");
        assertEq(arbToken.balanceOf(address(this)), targetProfit, "owner should get all profit");
        assertEq(arbToken.balanceOf(address(tipExecutor)), 0, "executor should have zero balance");
    }

    function test_executeArb_tipBps10000_allProfitToCoinbase() public {
        MockAavePool mockPool = new MockAavePool();
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12), address(0xBAAC));
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
        tipExecutor.executeArb(steps, address(arbToken), flashloanAmount, block.timestamp + 1000, 0, 10000);

        assertEq(arbToken.balanceOf(coinbase), targetProfit, "coinbase should get all profit");
        assertEq(arbToken.balanceOf(address(this)), 0, "owner should get nothing");
        assertEq(arbToken.balanceOf(address(tipExecutor)), 0, "executor should have zero balance");
    }

    function testFuzz_tipBps_profitSplit(uint256 tipBps) public {
        vm.assume(tipBps <= 10000);

        (AetherExecutor tipExecutor, MockERC20 arbToken) = _deployArbFixture(10_000);

        address coinbase = address(0xC01B);
        vm.coinbase(coinbase);

        tipExecutor.executeArb(_buildSingleStep(arbToken, 10_000), address(arbToken), 100_000, block.timestamp + 1000, 0, tipBps);

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
        wethExecutor.executeArb(steps, WETH_ADDR, 100_000, block.timestamp + 1000, 0, 9000);

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
        AetherExecutor tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12), address(0xBAAC));
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

        tipExecutor.executeArb(steps, address(arbToken), flashloanAmount, block.timestamp + 1000, 0, tipBps);

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
        tipExecutor = new AetherExecutor(address(mockPool), address(0xBA12), address(0xBAAC));
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
        wethExecutor = new AetherExecutor(address(mockPool), address(0xBA12), address(0xBAAC));

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
        executor.executeArb(steps, address(tokenIn), flashAmount, block.timestamp + 1000, 0, 0);

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

        executor.executeArb(steps, address(tokenIn), flashAmount, block.timestamp + 1000, 0, 0);

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

        executor.executeArb(steps, address(tokenIn), flashAmount, block.timestamp + 1000, 0, 0);

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

        executor.executeArb(steps, address(tokenIn), flashAmount, block.timestamp + 1000, 0, 0);

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
        AetherExecutor balExecutor = new AetherExecutor(address(aavePool), address(vault), address(0xBAAC));

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

        balExecutor.executeArb(steps, address(tokenIn), flashAmount, block.timestamp + 1000, 0, 0);

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

        // Deploy MockBancorRouter as the bancorNetwork — all Bancor trades route through
        // this single contract address, NOT through individual pool contracts.
        MockBancorRouter bancorNet = new MockBancorRouter(tokenIn, tokenOut, swapOut);
        tokenOut.mint(address(bancorNet), swapOut);

        // Deploy executor with bancorNetwork pointing at the mock router
        AetherExecutor bancorExecutor = new AetherExecutor(address(aavePool), address(0xBA12), address(bancorNet));

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
            address(bancorExecutor)
        );
        bytes memory returnData = abi.encodeWithSignature(
            "swap(uint256,uint256,address,bytes)",
            uint256(0),
            returnAmount,
            address(bancorExecutor),
            ""
        );

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](2);
        steps[0] = AetherExecutor.SwapStep({
            protocol: BANCOR_V3,
            pool: address(bancorNet), // individual pool address (unused by _swapBancor — only data matters)
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

        bancorExecutor.executeArb(steps, address(tokenIn), flashAmount, block.timestamp + 1000, 0, 0);

        // BancorNetwork (not individual pool) pulled tokens via transferFrom
        assertEq(tokenIn.balanceOf(address(bancorNet)), flashAmount);
        // Approval to bancorNetwork reset to 0 after swap
        assertEq(tokenIn.allowance(address(bancorExecutor), address(bancorNet)), 0);
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

        executor.executeArb(steps, address(tokenIn), flashAmount, block.timestamp + 1000, 0, 0);

        // Pool received tokens via direct transfer (same as UniV2)
        assertEq(tokenIn.balanceOf(address(pool)), flashAmount);
        assertGt(tokenIn.balanceOf(owner), 0);
    }

    // =========================================================================
    //                    DEX REGISTRY + PAUSE + OWNABLE2STEP
    //                    + SECURITY-FIX COVERAGE (PR1 / E4-WS3)
    // =========================================================================
    //
    // This block covers the runtime DEX registry (setDexRouter/setDexEnabled),
    // the pause circuit breaker, the full Ownable2Step handoff, and the three
    // security fixes that shipped with the registry change:
    //   1) rescue() now sends native ETH when token==address(0)
    //   2) _executeSwap caps UniV2/Sushi amountIn at the executor's live balance
    //   3) coinbase tip falls back to WETH-transfer when block.coinbase rejects ETH
    // Protocol-constant parity with the Rust ProtocolType enum is sentinel-checked
    // here; the authoritative discriminant test lives in crates/common (PR2).
    // =========================================================================

    // Re-declare registry events for vm.expectEmit matching
    event DexRouterSet(uint8 indexed protocol, address router);
    event DexEnabledSet(uint8 indexed protocol, bool enabled);
    event PausedSet(bool paused);

    // -------------------------------------------------------------------------
    // Registry — setDexRouter
    // -------------------------------------------------------------------------

    function test_setDexRouter_onlyOwner() public {
        address intruder = address(0x456);
        vm.prank(intruder);
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, intruder)
        );
        executor.setDexRouter(BALANCER_V2, address(0xBEEF));
    }

    function test_setDexRouter_updatesMappingAndEmits() public {
        address newVault = address(0xB0B);

        vm.expectEmit(true, false, false, true);
        emit DexRouterSet(BALANCER_V2, newVault);

        executor.setDexRouter(BALANCER_V2, newVault);

        assertEq(executor.protocolRouter(BALANCER_V2), newVault, "router not updated");
    }

    /// @dev End-to-end: setDexRouter(BALANCER_V2, secondVault) must route the next Balancer
    ///      hop to the NEW vault and leave the original vault untouched.
    function test_setDexRouter_balancerV2_routesToNewVault() public {
        MockERC20 tokenIn = new MockERC20();
        MockERC20 tokenOut = new MockERC20();

        uint256 flashAmount = 1000;
        uint256 swapOut = 1100;
        uint256 premium = flashAmount * 5 / 10000;

        // Deploy two vaults — both can serve the swap; we assert only the second is called.
        CountingBalancerVault firstVault = new CountingBalancerVault(tokenIn, tokenOut, swapOut);
        CountingBalancerVault secondVault = new CountingBalancerVault(tokenIn, tokenOut, swapOut);

        // Seed both vaults with enough tokenOut to complete the swap.
        tokenOut.mint(address(firstVault), swapOut);
        tokenOut.mint(address(secondVault), swapOut);

        // Deploy executor pointing at firstVault.
        AetherExecutor regExecutor = new AetherExecutor(address(aavePool), address(firstVault), address(0xBAAC));

        // Migrate to secondVault — this is the action under test.
        regExecutor.setDexRouter(BALANCER_V2, address(secondVault));
        assertEq(regExecutor.protocolRouter(BALANCER_V2), address(secondVault), "router not updated");

        // Return pool to close the arb loop.
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
            address(regExecutor),
            ""
        );

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](2);
        steps[0] = AetherExecutor.SwapStep({
            protocol: BALANCER_V2,
            pool: address(secondVault), // pool field is informational for Balancer; router drives the call
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

        regExecutor.executeArb(steps, address(tokenIn), flashAmount, block.timestamp + 1000, 0, 0);

        // Only the second vault must have been called.
        assertEq(secondVault.swapCallCount(), 1, "secondVault must receive exactly one swap call");
        assertEq(firstVault.swapCallCount(), 0, "firstVault must not be called after router migration");
        // Approval to the new vault must be reset to 0 after the swap.
        assertEq(tokenIn.allowance(address(regExecutor), address(secondVault)), 0, "approval not reset");
    }

    function test_setDexRouter_revert_zeroRouter() public {
        vm.expectRevert(AetherExecutor.ZeroRouter.selector);
        executor.setDexRouter(BALANCER_V2, address(0));
    }

    function test_setDexRouter_revert_unknownProtocol() public {
        vm.expectRevert(abi.encodeWithSelector(AetherExecutor.UnknownProtocol.selector, uint8(0)));
        executor.setDexRouter(0, address(0xBEEF));

        vm.expectRevert(abi.encodeWithSelector(AetherExecutor.UnknownProtocol.selector, uint8(7)));
        executor.setDexRouter(7, address(0xBEEF));
    }

    // -------------------------------------------------------------------------
    // Registry — setDexEnabled
    // -------------------------------------------------------------------------

    function test_setDexEnabled_onlyOwner() public {
        address intruder = address(0x456);
        vm.prank(intruder);
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, intruder)
        );
        executor.setDexEnabled(CURVE, false);
    }

    function test_setDexEnabled_togglesAndEmits() public {
        // Default is true; flip off, then back on. Each transition emits exactly one event.
        vm.expectEmit(true, false, false, true);
        emit DexEnabledSet(CURVE, false);
        executor.setDexEnabled(CURVE, false);
        assertFalse(executor.protocolEnabled(CURVE), "curve should be disabled");

        vm.expectEmit(true, false, false, true);
        emit DexEnabledSet(CURVE, true);
        executor.setDexEnabled(CURVE, true);
        assertTrue(executor.protocolEnabled(CURVE), "curve should be re-enabled");
    }

    function test_setDexEnabled_idempotent_noEvent() public {
        // CURVE defaults to true in the constructor. Writing true again must be a no-op:
        // no storage write, no event.
        vm.recordLogs();
        executor.setDexEnabled(CURVE, true);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        assertEq(logs.length, 0, "idempotent setDexEnabled must not emit");
        assertTrue(executor.protocolEnabled(CURVE), "curve still enabled");
    }

    function test_setDexEnabled_revert_unknownProtocol() public {
        vm.expectRevert(abi.encodeWithSelector(AetherExecutor.UnknownProtocol.selector, uint8(0)));
        executor.setDexEnabled(0, true);

        vm.expectRevert(abi.encodeWithSelector(AetherExecutor.UnknownProtocol.selector, uint8(7)));
        executor.setDexEnabled(7, true);
    }

    // -------------------------------------------------------------------------
    // Pre-flashloan validation (CRITICAL — disabled/unknown protocol must fail
    // fast before Aave fires, otherwise we owe premium on a doomed tx)
    // -------------------------------------------------------------------------

    function test_executeArb_revert_protocolDisabled_preFlashloan() public {
        // Use a counting Aave pool so we can prove the revert fired BEFORE flashLoanSimple.
        CountingAavePool countingPool = new CountingAavePool();
        AetherExecutor gatedExecutor = new AetherExecutor(
            address(countingPool), address(0xBA12), address(0xBAAC)
        );

        gatedExecutor.setDexEnabled(CURVE, false);

        // 3-hop arb with CURVE in the middle hop — revert should cite hop-2's protocol.
        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](3);
        steps[0] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(0xAA01),
            tokenIn: address(token),
            tokenOut: address(token2),
            amountIn: 1,
            minAmountOut: 1,
            data: ""
        });
        steps[1] = AetherExecutor.SwapStep({
            protocol: CURVE,
            pool: address(0xAA02),
            tokenIn: address(token2),
            tokenOut: address(token),
            amountIn: 1,
            minAmountOut: 1,
            data: ""
        });
        steps[2] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(0xAA03),
            tokenIn: address(token),
            tokenOut: address(token2),
            amountIn: 1,
            minAmountOut: 1,
            data: ""
        });

        vm.expectRevert(abi.encodeWithSelector(AetherExecutor.ProtocolDisabled.selector, CURVE));
        gatedExecutor.executeArb(steps, address(token), 1000, block.timestamp + 1000, 0, 0);

        assertEq(countingPool.flashLoanCallCount(), 0, "flashloan must not fire when pre-check rejects");
    }

    function test_executeArb_revert_unknownProtocol_preFlashloan() public {
        CountingAavePool countingPool = new CountingAavePool();
        AetherExecutor gatedExecutor = new AetherExecutor(
            address(countingPool), address(0xBA12), address(0xBAAC)
        );

        // protocol = 0 rejected
        AetherExecutor.SwapStep[] memory stepsZero = new AetherExecutor.SwapStep[](1);
        stepsZero[0] = AetherExecutor.SwapStep({
            protocol: 0,
            pool: address(0xAA01),
            tokenIn: address(token),
            tokenOut: address(token2),
            amountIn: 1,
            minAmountOut: 1,
            data: ""
        });
        vm.expectRevert(abi.encodeWithSelector(AetherExecutor.UnknownProtocol.selector, uint8(0)));
        gatedExecutor.executeArb(stepsZero, address(token), 1000, block.timestamp + 1000, 0, 0);

        // protocol = 7 rejected
        AetherExecutor.SwapStep[] memory stepsSeven = new AetherExecutor.SwapStep[](1);
        stepsSeven[0] = AetherExecutor.SwapStep({
            protocol: 7,
            pool: address(0xAA01),
            tokenIn: address(token),
            tokenOut: address(token2),
            amountIn: 1,
            minAmountOut: 1,
            data: ""
        });
        vm.expectRevert(abi.encodeWithSelector(AetherExecutor.UnknownProtocol.selector, uint8(7)));
        gatedExecutor.executeArb(stepsSeven, address(token), 1000, block.timestamp + 1000, 0, 0);

        assertEq(countingPool.flashLoanCallCount(), 0, "flashloan must not fire on unknown protocol");
    }

    // -------------------------------------------------------------------------
    // Pause circuit breaker
    // -------------------------------------------------------------------------

    function test_setPaused_onlyOwner() public {
        address intruder = address(0x456);
        vm.prank(intruder);
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, intruder)
        );
        executor.setPaused(true);
    }

    function test_setPaused_emitsEvent_and_idempotent() public {
        assertFalse(executor.paused(), "starts unpaused");

        // false -> true emits
        vm.expectEmit(false, false, false, true);
        emit PausedSet(true);
        executor.setPaused(true);
        assertTrue(executor.paused(), "paused after flip");

        // true -> true is a no-op (no event)
        vm.recordLogs();
        executor.setPaused(true);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        assertEq(logs.length, 0, "idempotent setPaused must not emit");

        // true -> false emits
        vm.expectEmit(false, false, false, true);
        emit PausedSet(false);
        executor.setPaused(false);
        assertFalse(executor.paused(), "unpaused after flip-back");
    }

    function test_executeArb_revert_whenPaused() public {
        executor.setPaused(true);

        AetherExecutor.SwapStep[] memory steps = new AetherExecutor.SwapStep[](1);
        steps[0] = AetherExecutor.SwapStep({
            protocol: UNISWAP_V2,
            pool: address(0xAA01),
            tokenIn: address(token),
            tokenOut: address(token2),
            amountIn: 1,
            minAmountOut: 1,
            data: ""
        });

        vm.expectRevert(AetherExecutor.Paused.selector);
        executor.executeArb(steps, address(token), 1000, block.timestamp + 1000, 0, 0);
    }

    // -------------------------------------------------------------------------
    // Ownable2Step — two-step transfer semantics
    // -------------------------------------------------------------------------

    function test_twoStep_transfer_requires_acceptance() public {
        address newOwner = address(0xAAA1);

        // Step 1: nominate
        executor.transferOwnership(newOwner);
        assertEq(executor.owner(), owner, "owner unchanged by step 1");
        assertEq(executor.pendingOwner(), newOwner, "pendingOwner set");

        // Step 2: accept (must be called by nominee)
        vm.prank(newOwner);
        executor.acceptOwnership();

        assertEq(executor.owner(), newOwner, "owner updated after acceptance");
        assertEq(executor.pendingOwner(), address(0), "pendingOwner cleared");
    }

    function test_acceptOwnership_revert_notPending() public {
        // No pending transfer in flight — any caller is "not pending".
        address randomAddr = address(0xD00D);
        vm.prank(randomAddr);
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, randomAddr)
        );
        executor.acceptOwnership();
    }

    // -------------------------------------------------------------------------
    // Security fix 1 — rescue() handles native ETH
    // -------------------------------------------------------------------------

    function test_rescue_eth() public {
        vm.deal(address(executor), 1 ether);
        assertEq(address(executor).balance, 1 ether);

        uint256 ownerBalBefore = owner.balance;
        executor.rescue(address(0), 1 ether);

        assertEq(address(executor).balance, 0, "executor should have no ETH left");
        assertEq(owner.balance - ownerBalBefore, 1 ether, "owner delta must equal rescued ETH");
    }

    function test_rescue_eth_onlyOwner() public {
        vm.deal(address(executor), 1 ether);
        address intruder = address(0x456);
        vm.prank(intruder);
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, intruder)
        );
        executor.rescue(address(0), 1 ether);
    }

    // -------------------------------------------------------------------------
    // Security fix 2 — _executeSwap caps UniV2/Sushi transfer at live balance
    //
    // If the off-chain optimizer over-spec's amountIn, executor must clamp to its
    // actual balance rather than reverting or transferring more than it owns.
    // -------------------------------------------------------------------------

    function test_swapUniV2_capsAtBalance_whenAmountInExceedsBalance() public {
        MockERC20 tokenIn = new MockERC20();
        MockERC20 tokenOut = new MockERC20();

        // Flash-loan amount = what the executor actually receives from Aave.
        uint256 flashAmount = 500;
        // Over-spec: claim we can swap 1000 but only 500 are on-hand.
        uint256 overSpecAmountIn = 1000;

        uint256 swapOut = 1100;
        uint256 premium = (flashAmount * 5) / 10000;

        MockV2Pool pool = new MockV2Pool(tokenIn, tokenOut, swapOut);
        tokenOut.mint(address(pool), swapOut);

        // Return hop converts tokenOut -> tokenIn with enough output to repay + leave profit.
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
            protocol: UNISWAP_V2,
            pool: address(pool),
            tokenIn: address(tokenIn),
            tokenOut: address(tokenOut),
            amountIn: overSpecAmountIn, // 1000 requested, only 500 live
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

        executor.executeArb(steps, address(tokenIn), flashAmount, block.timestamp + 1000, 0, 0);

        // Cap kicked in: pool received the live balance, not the over-spec figure.
        assertEq(tokenIn.balanceOf(address(pool)), flashAmount, "pool should receive capped (live-balance) amount");
        assertTrue(flashAmount < overSpecAmountIn, "sanity: over-spec > live balance");
    }

    // -------------------------------------------------------------------------
    // Security fix 3 — coinbase tip falls back to WETH on reverting coinbase
    //
    // Some builders run contract-coinbases whose receive() reverts. The executor
    // must recover by re-wrapping the ETH and transferring WETH to the coinbase.
    // We assert the fallback ran by checking the coinbase's post-tx WETH balance.
    // -------------------------------------------------------------------------

    function test_coinbaseTip_fallsBackToWeth_onRevertingCoinbase() public {
        address WETH_ADDR = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
        _deployMockWethAt(WETH_ADDR);

        (AetherExecutor wethExecutor, AetherExecutor.SwapStep[] memory steps) =
            _buildWethArbFixture(WETH_ADDR, 1000);

        // Contract coinbase whose receive() reverts — forces the WETH fallback path.
        RevertingCoinbase revertingCB = new RevertingCoinbase();
        vm.coinbase(address(revertingCB));

        vm.deal(WETH_ADDR, 10_000);

        uint256 tipBps = 9000; // tip = 900
        uint256 expectedTip = 900;
        uint256 expectedOwner = 100;

        wethExecutor.executeArb(steps, WETH_ADDR, 100_000, block.timestamp + 1000, 0, tipBps);

        // Native ETH transfer to the reverting coinbase must have failed, so executor
        // re-wrapped and ERC20-transferred WETH instead. Balance proves the fallback ran.
        assertEq(
            MockWETH(payable(WETH_ADDR)).balanceOf(address(revertingCB)),
            expectedTip,
            "coinbase should receive WETH via fallback"
        );
        assertEq(address(revertingCB).balance, 0, "no native ETH should reach reverting coinbase");
        assertEq(
            MockWETH(payable(WETH_ADDR)).balanceOf(address(this)),
            expectedOwner,
            "owner WETH profit incorrect"
        );
        assertEq(
            MockWETH(payable(WETH_ADDR)).balanceOf(address(wethExecutor)),
            0,
            "executor should have zero WETH leftover"
        );
    }

    // -------------------------------------------------------------------------
    // Solidity ↔ Rust invariant sentinel
    //
    // The Solidity protocol constants are private, so we can't read them directly.
    // Instead we assert the constructor-seeded enabled set exactly matches the
    // expected range [1..=BANCOR_V3]. This is a weak-but-nonzero sentinel; the
    // authoritative check lives Rust-side in crates/common/src/types.rs (PR2) via
    // a discriminant-equality test against these same ids.
    // -------------------------------------------------------------------------

    function test_protocolConstants_implicitlyMatch() public view {
        // In-range (1..=6) must all be enabled at construction.
        // Hardcoded numeric IDs — using the test-file constants here would make
        // the sentinel circular: if a constant drifted, the test would still pass.
        assertTrue(executor.protocolEnabled(1), "UNISWAP_V2 (1)");
        assertTrue(executor.protocolEnabled(2), "UNISWAP_V3 (2)");
        assertTrue(executor.protocolEnabled(3), "SUSHISWAP (3)");
        assertTrue(executor.protocolEnabled(4), "CURVE (4)");
        assertTrue(executor.protocolEnabled(5), "BALANCER_V2 (5)");
        assertTrue(executor.protocolEnabled(6), "BANCOR_V3 (6)");

        // Out-of-range (0 and >=7) must stay default-false.
        assertFalse(executor.protocolEnabled(0), "protocol 0 must be disabled");
        assertFalse(executor.protocolEnabled(7), "protocol 7 must be disabled");
        assertFalse(executor.protocolEnabled(255), "protocol 255 must be disabled");
    }
}
