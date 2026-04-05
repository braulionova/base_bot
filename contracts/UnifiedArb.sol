// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title UnifiedArb — Flash loan arb + liquidation in one contract
/// @notice Combines cross-DEX arb (direct + triangular) with Aave V3 liquidations
/// @dev All operations use Aave flash loans for zero-capital execution

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
}

interface IPool {
    function flashLoanSimple(address, address, uint256, bytes calldata, uint16) external;
    function liquidationCall(address collateral, address debt, address user, uint256 amount, bool receiveAToken) external;
}

interface IV3 {
    function swap(address, bool, int256, uint160, bytes calldata) external returns (int256, int256);
    function token0() external view returns (address);
}

interface IV2 {
    function swap(uint, uint, address, bytes calldata) external;
    function getReserves() external view returns (uint112, uint112, uint32);
    function token0() external view returns (address);
}

contract UnifiedArb {
    address public immutable owner;
    address constant AAVE = 0xA238Dd80C259a72e81d7e4664a9801593F98d1c5;
    uint160 constant MIN_S = 4295128739;
    uint160 constant MAX_S = 1461446703485210103287273052203988822378723970342;

    // Op types for flash loan callback routing
    uint8 constant OP_DIRECT = 1;
    uint8 constant OP_TRIANGULAR = 2;
    uint8 constant OP_LIQUIDATION = 3;

    constructor() { owner = msg.sender; }

    modifier onlyOwner() {
        require(msg.sender == owner, "!o");
        _;
    }

    // ================================================================
    // ENTRY POINTS
    // ================================================================

    /// @notice Direct cross-DEX arb: borrow tokenIn, buy on poolA, sell on poolB
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
            OP_DIRECT, poolA, poolB, poolAisV3, poolBisV3, tokenBridge,
            address(0), address(0), false, address(0) // unused tri/liq fields
        );
        IPool(AAVE).flashLoanSimple(address(this), tokenIn, amountIn, params, 0);
    }

    /// @notice Triangular arb: borrow WETH → tokenA → tokenB → WETH
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
            OP_TRIANGULAR, pool1, pool2, pool1isV3, pool2isV3, tokenA,
            pool3, tokenB, pool3isV3, address(0)
        );
        IPool(AAVE).flashLoanSimple(address(this), weth, amountIn, params, 0);
    }

    /// @notice Liquidation: flash loan debt asset, liquidate user, receive collateral, swap back
    /// @param debtAsset The debt token to borrow
    /// @param debtAmount Amount of debt to cover (up to 50% of user's debt)
    /// @param collateralAsset The collateral to receive
    /// @param user The underwater user to liquidate
    /// @param swapPool Pool to swap collateral → debt asset to repay flash loan
    /// @param swapIsV3 Whether the swap pool is V3
    function execLiquidation(
        address debtAsset,
        uint256 debtAmount,
        address collateralAsset,
        address user,
        address swapPool,
        bool swapIsV3
    ) external onlyOwner {
        bytes memory params = abi.encode(
            OP_LIQUIDATION,
            collateralAsset, swapPool, swapIsV3, false, debtAsset,
            user, address(0), false, address(0)
        );
        IPool(AAVE).flashLoanSimple(address(this), debtAsset, debtAmount, params, 0);
    }

    // ================================================================
    // AAVE FLASH LOAN CALLBACK
    // ================================================================

    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address,
        bytes calldata params
    ) external returns (bool) {
        require(msg.sender == AAVE, "!aave");

        (
            uint8 opType,
            address addr1, address addr2,
            bool flag1, bool flag2, address token1,
            address addr3, address token2, bool flag3,
            address /* unused */
        ) = abi.decode(params, (uint8, address, address, bool, bool, address, address, address, bool, address));

        if (opType == OP_DIRECT) {
            _execDirect(asset, amount, addr1, addr2, flag1, flag2, token1);
        } else if (opType == OP_TRIANGULAR) {
            _execTriangular(amount, addr1, addr2, flag1, flag2, token1, addr3, token2, flag3);
        } else if (opType == OP_LIQUIDATION) {
            _execLiquidation(asset, amount, addr1, token1, addr3, addr2, flag1);
        }

        // Repay flash loan
        IERC20(asset).approve(AAVE, amount + premium);
        return true;
    }

    // ================================================================
    // INTERNAL EXECUTION
    // ================================================================

    function _execDirect(
        address tokenIn, uint256 amountIn,
        address poolA, address poolB,
        bool poolAisV3, bool poolBisV3,
        address tokenBridge
    ) internal {
        // Leg 1: tokenIn → tokenBridge on poolA
        uint256 mid;
        if (poolAisV3) mid = _swapV3(poolA, tokenIn, tokenBridge, amountIn);
        else mid = _swapV2(poolA, tokenIn, tokenBridge, amountIn);

        // Leg 2: tokenBridge → tokenIn on poolB
        if (poolBisV3) _swapV3(poolB, tokenBridge, tokenIn, mid);
        else _swapV2(poolB, tokenBridge, tokenIn, mid);
    }

    function _execTriangular(
        uint256 amountIn,
        address pool1, address pool2,
        bool pool1isV3, bool pool2isV3,
        address tokenA,
        address pool3, address tokenB, bool pool3isV3
    ) internal {
        address weth = 0x4200000000000000000000000000000000000006;

        uint256 a;
        if (pool1isV3) a = _swapV3(pool1, weth, tokenA, amountIn);
        else a = _swapV2(pool1, weth, tokenA, amountIn);

        uint256 b;
        if (pool2isV3) b = _swapV3(pool2, tokenA, tokenB, a);
        else b = _swapV2(pool2, tokenA, tokenB, a);

        if (pool3isV3) _swapV3(pool3, tokenB, weth, b);
        else _swapV2(pool3, tokenB, weth, b);
    }

    function _execLiquidation(
        address debtAsset, uint256 debtAmount,
        address collateralAsset, address /* debtAssetAgain */,
        address user, address swapPool, bool swapIsV3
    ) internal {
        // Step 1: Approve Aave to pull debt tokens for liquidation
        IERC20(debtAsset).approve(AAVE, debtAmount);

        // Step 2: Execute liquidation — receive collateral tokens
        IPool(AAVE).liquidationCall(
            collateralAsset, debtAsset, user, debtAmount, false
        );

        // Step 3: Swap ALL received collateral back to debt asset
        uint256 collateralReceived = IERC20(collateralAsset).balanceOf(address(this));
        if (collateralReceived > 0) {
            if (swapIsV3) _swapV3(swapPool, collateralAsset, debtAsset, collateralReceived);
            else _swapV2(swapPool, collateralAsset, debtAsset, collateralReceived);
        }
        // Profit = debt asset balance - (debtAmount + flash loan premium)
        // Flash loan callback will handle repayment
    }

    // ================================================================
    // SWAP HELPERS
    // ================================================================

    function _swapV3(address pool, address tIn, address tOut, uint256 amt) internal returns (uint256) {
        IERC20(tIn).approve(pool, amt);
        bool zf = tIn < tOut;
        (int256 a0, int256 a1) = IV3(pool).swap(
            address(this), zf, int256(amt), zf ? MIN_S + 1 : MAX_S - 1,
            abi.encode(tIn, amt)
        );
        return uint256(-(zf ? a1 : a0));
    }

    function _swapV2(address pair, address tIn, address, uint256 amt) internal returns (uint256) {
        IERC20(tIn).transfer(pair, amt);
        (uint112 r0, uint112 r1,) = IV2(pair).getReserves();
        address t0 = IV2(pair).token0();
        (uint256 rI, uint256 rO) = tIn == t0
            ? (uint256(r0), uint256(r1))
            : (uint256(r1), uint256(r0));
        uint256 a = amt * 997;
        uint256 out = (a * rO) / (rI * 1000 + a);
        (uint256 o0, uint256 o1) = tIn == t0 ? (uint256(0), out) : (out, uint256(0));
        IV2(pair).swap(o0, o1, address(this), "");
        return out;
    }

    // ================================================================
    // V3 CALLBACKS (tokens sent via approve before swap)
    // ================================================================

    function uniswapV3SwapCallback(int256 a0, int256 a1, bytes calldata data) external {
        _v3Callback(a0, a1, data);
    }
    function pancakeV3SwapCallback(int256 a0, int256 a1, bytes calldata data) external {
        _v3Callback(a0, a1, data);
    }
    function algebraSwapCallback(int256 a0, int256 a1, bytes calldata data) external {
        _v3Callback(a0, a1, data);
    }
    function ramsesV2SwapCallback(int256 a0, int256 a1, bytes calldata data) external {
        _v3Callback(a0, a1, data);
    }

    function _v3Callback(int256 a0, int256 a1, bytes calldata data) internal {
        (address tokenIn,) = abi.decode(data, (address, uint256));
        uint256 owed = a0 > 0 ? uint256(a0) : uint256(a1);
        IERC20(tokenIn).transfer(msg.sender, owed);
    }

    // ================================================================
    // ADMIN
    // ================================================================

    function withdraw(address token) external onlyOwner {
        IERC20(token).transfer(owner, IERC20(token).balanceOf(address(this)));
    }

    function withdrawETH() external onlyOwner {
        (bool ok,) = owner.call{value: address(this).balance}("");
        require(ok);
    }

    receive() external payable {}
}
