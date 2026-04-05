// SPDX-License-Identifier: MIT
pragma solidity ^0.8.19;

/// @title FlashArbV2 — Aave V3 Flash Loan Arbitrage (V2 + V3 pools, Direct + Triangular)
/// @notice Borrows via Aave flash loan, executes multi-leg swaps, repays, keeps profit.

interface IPoolAddressesProvider {
    function getPool() external view returns (address);
}

interface IAavePool {
    function flashLoanSimple(
        address receiverAddress,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 referralCode
    ) external;
}

interface IUniswapV2Pair {
    function swap(uint amount0Out, uint amount1Out, address to, bytes calldata data) external;
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32);
    function token0() external view returns (address);
    function token1() external view returns (address);
}

interface IUniswapV3Pool {
    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
}

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
    function transferFrom(address, address, uint256) external returns (bool);
}

interface IUniswapV3SwapCallback {
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external;
}

contract FlashArbV2 is IUniswapV3SwapCallback {
    address public immutable owner;
    IAavePool public immutable aavePool;

    // Min sqrt price limits for V3 swaps
    uint160 internal constant MIN_SQRT_RATIO = 4295128739;
    uint160 internal constant MAX_SQRT_RATIO = 1461446703485210103287273052203988822378723970342;

    // Operation types encoded in flash loan params
    uint8 internal constant OP_DIRECT = 1;
    uint8 internal constant OP_TRIANGULAR = 2;

    // Pool type flags
    uint8 internal constant POOL_V2 = 0;
    uint8 internal constant POOL_V3 = 1;

    modifier onlyOwner() {
        require(msg.sender == owner, "!owner");
        _;
    }

    constructor(address _aavePoolProvider) {
        owner = msg.sender;
        aavePool = IAavePool(IPoolAddressesProvider(_aavePoolProvider).getPool());
    }

    /// @notice Execute direct arb: borrow tokenIn, swap on poolA, swap back on poolB
    /// @param tokenIn Token to borrow (usually WETH)
    /// @param amountIn Amount to flash loan
    /// @param poolA First pool (buy leg)
    /// @param poolB Second pool (sell leg)
    /// @param poolAisV3 Whether poolA is V3
    /// @param poolBisV3 Whether poolB is V3
    /// @param tokenBridge Intermediate token
    function execDirect(
        address tokenIn,
        uint256 amountIn,
        address poolA,
        address poolB,
        bool poolAisV3,
        bool poolBisV3,
        address tokenBridge
    ) external onlyOwner {
        bytes memory params = abi.encode(
            OP_DIRECT,
            poolA, poolB,
            poolAisV3, poolBisV3,
            tokenIn, tokenBridge,
            address(0), false, address(0) // unused triangular fields
        );
        aavePool.flashLoanSimple(address(this), tokenIn, amountIn, params, 0);
    }

    /// @notice Execute triangular arb: borrow WETH, swap through 3 pools
    /// @param amountIn Amount to flash loan (in WETH)
    /// @param pool1 WETH -> tokenA
    /// @param pool2 tokenA -> tokenB
    /// @param pool3 tokenB -> WETH
    /// @param pool1isV3 Whether pool1 is V3
    /// @param pool2isV3 Whether pool2 is V3
    /// @param pool3isV3 Whether pool3 is V3
    /// @param tokenA First intermediate token
    /// @param tokenB Second intermediate token
    function execTriangular(
        uint256 amountIn,
        address pool1,
        address pool2,
        address pool3,
        bool pool1isV3,
        bool pool2isV3,
        bool pool3isV3,
        address tokenA,
        address tokenB
    ) external onlyOwner {
        address weth = 0x4200000000000000000000000000000000000006;
        bytes memory params = abi.encode(
            OP_TRIANGULAR,
            pool1, pool2,
            pool1isV3, pool2isV3,
            weth, tokenA,
            pool3, pool3isV3, tokenB
        );
        aavePool.flashLoanSimple(address(this), weth, amountIn, params, 0);
    }

    /// @notice Aave flash loan callback
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address /* initiator */,
        bytes calldata params
    ) external returns (bool) {
        require(msg.sender == address(aavePool), "!aave");

        (
            uint8 opType,
            address pool1, address pool2,
            bool pool1isV3, bool pool2isV3,
            address tokenIn, address tokenBridge,
            address pool3, bool pool3isV3, address tokenB
        ) = abi.decode(params, (uint8, address, address, bool, bool, address, address, address, bool, address));

        if (opType == OP_DIRECT) {
            _executeDirect(tokenIn, amount, pool1, pool2, pool1isV3, pool2isV3, tokenBridge);
        } else if (opType == OP_TRIANGULAR) {
            _executeTriangular(amount, pool1, pool2, pool3, pool1isV3, pool2isV3, pool3isV3, tokenBridge, tokenB);
        }

        // Repay flash loan (amount + premium)
        uint256 repayAmount = amount + premium;
        IERC20(asset).approve(address(aavePool), repayAmount);

        return true;
    }

    function _executeDirect(
        address tokenIn,
        uint256 amountIn,
        address poolA,
        address poolB,
        bool poolAisV3,
        bool poolBisV3,
        address tokenBridge
    ) internal {
        // Leg 1: tokenIn -> tokenBridge on poolA
        uint256 bridgeAmount;
        if (poolAisV3) {
            bridgeAmount = _swapV3(poolA, tokenIn, tokenBridge, amountIn);
        } else {
            bridgeAmount = _swapV2(poolA, tokenIn, tokenBridge, amountIn);
        }

        // Leg 2: tokenBridge -> tokenIn on poolB
        if (poolBisV3) {
            _swapV3(poolB, tokenBridge, tokenIn, bridgeAmount);
        } else {
            _swapV2(poolB, tokenBridge, tokenIn, bridgeAmount);
        }
    }

    function _executeTriangular(
        uint256 amountIn,
        address pool1,
        address pool2,
        address pool3,
        bool pool1isV3,
        bool pool2isV3,
        bool pool3isV3,
        address tokenA,
        address tokenB
    ) internal {
        address weth = 0x4200000000000000000000000000000000000006;

        // Leg 1: WETH -> tokenA
        uint256 amountA;
        if (pool1isV3) {
            amountA = _swapV3(pool1, weth, tokenA, amountIn);
        } else {
            amountA = _swapV2(pool1, weth, tokenA, amountIn);
        }

        // Leg 2: tokenA -> tokenB
        uint256 amountB;
        if (pool2isV3) {
            amountB = _swapV3(pool2, tokenA, tokenB, amountA);
        } else {
            amountB = _swapV2(pool2, tokenA, tokenB, amountA);
        }

        // Leg 3: tokenB -> WETH
        if (pool3isV3) {
            _swapV3(pool3, tokenB, weth, amountB);
        } else {
            _swapV2(pool3, tokenB, weth, amountB);
        }
    }

    function _swapV2(
        address pair,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        // Transfer tokens to pair BEFORE calling swap (avoids callback issues)
        IERC20(tokenIn).transfer(pair, amountIn);

        (uint112 r0, uint112 r1,) = IUniswapV2Pair(pair).getReserves();
        address t0 = IUniswapV2Pair(pair).token0();

        (uint256 resIn, uint256 resOut) = tokenIn == t0
            ? (uint256(r0), uint256(r1))
            : (uint256(r1), uint256(r0));

        uint256 amountInWithFee = amountIn * 997;
        amountOut = (amountInWithFee * resOut) / (resIn * 1000 + amountInWithFee);

        (uint256 out0, uint256 out1) = tokenIn == t0
            ? (uint256(0), amountOut)
            : (amountOut, uint256(0));

        IUniswapV2Pair(pair).swap(out0, out1, address(this), "");
    }

    function _swapV3(
        address pool,
        address tokenIn,
        address /* tokenOut */,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        address t0 = IUniswapV3Pool(pool).token0();
        bool zeroForOne = (tokenIn == t0);

        uint160 sqrtLimit = zeroForOne ? MIN_SQRT_RATIO + 1 : MAX_SQRT_RATIO - 1;

        // Approve pool to pull tokens via callback
        IERC20(tokenIn).approve(pool, amountIn);

        (int256 amount0, int256 amount1) = IUniswapV3Pool(pool).swap(
            address(this),
            zeroForOne,
            int256(amountIn),
            sqrtLimit,
            abi.encode(tokenIn, amountIn)
        );

        // Output is the negative delta
        amountOut = uint256(-(zeroForOne ? amount1 : amount0));
    }

    /// @notice V3 swap callback — sends tokens to pool
    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external override {
        (address tokenIn,) = abi.decode(data, (address, uint256));

        uint256 amountOwed = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        IERC20(tokenIn).transfer(msg.sender, amountOwed);
    }

    /// @notice Withdraw profits
    function withdraw(address token) external onlyOwner {
        uint256 bal = IERC20(token).balanceOf(address(this));
        if (bal > 0) {
            IERC20(token).transfer(owner, bal);
        }
    }

    /// @notice Withdraw ETH
    function withdrawETH() external onlyOwner {
        uint256 bal = address(this).balance;
        if (bal > 0) {
            payable(owner).transfer(bal);
        }
    }

    receive() external payable {}
}
