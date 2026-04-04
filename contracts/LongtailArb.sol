// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

// V2 Flash Swap Arb - TRUE zero capital
// 1. Call V2 pair.swap() with data -> pair sends tokens FIRST
// 2. In callback: sell tokens on another pool
// 3. Repay V2 pair
// 4. Keep profit

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
}

interface IV2Pair {
    function swap(uint, uint, address, bytes calldata) external;
    function getReserves() external view returns (uint112, uint112, uint32);
    function token0() external view returns (address);
    function token1() external view returns (address);
}

interface IV3Pool {
    function swap(address, bool, int256, uint160, bytes calldata) external returns (int256, int256);
    function token0() external view returns (address);
    function token1() external view returns (address);
}

contract LongtailArb {
    address public immutable owner;
    uint160 constant MIN_SQRT = 4295128739;
    uint160 constant MAX_SQRT = 1461446703485210103287273052203988822378723970342;

    // Callback context
    address private _sellPool;
    address private _tokenBorrow;
    address private _tokenPay;
    bool private _sellV3;
    uint256 private _repayAmount;

    constructor() { owner = msg.sender; }

    /// @notice Flash arb using V2 flash swap (ZERO capital)
    /// @param v2Pair V2 pair to flash-borrow from
    /// @param sellPool Pool to sell borrowed tokens on (V2 or V3)
    /// @param borrowToken Token to borrow (the one with price diff)
    /// @param borrowAmount Amount to flash-borrow
    /// @param sellOnV3 Is sellPool a V3 pool?
    function exec(
        address v2Pair,
        address sellPool,
        address borrowToken,
        uint256 borrowAmount,
        bool sellOnV3
    ) external {
        require(msg.sender == owner, "!o");

        address t0 = IV2Pair(v2Pair).token0();
        address t1 = IV2Pair(v2Pair).token1();

        // Store context
        _sellPool = sellPool;
        _tokenBorrow = borrowToken;
        _tokenPay = borrowToken == t0 ? t1 : t0;
        _sellV3 = sellOnV3;

        // Calculate how much we need to repay (with 0.3% fee)
        (uint112 r0, uint112 r1,) = IV2Pair(v2Pair).getReserves();
        (uint256 rBorrow, uint256 rPay) = borrowToken == t0
            ? (uint256(r0), uint256(r1))
            : (uint256(r1), uint256(r0));
        // repayAmount = (borrowAmount * rPay * 1000) / ((rBorrow - borrowAmount) * 997) + 1
        _repayAmount = (borrowAmount * rPay * 1000) / ((rBorrow - borrowAmount) * 997) + 1;

        // Flash swap: request borrowToken, pair sends it FIRST then calls uniswapV2Call
        (uint256 out0, uint256 out1) = borrowToken == t0
            ? (borrowAmount, uint256(0))
            : (uint256(0), borrowAmount);

        // Non-empty data triggers flash swap callback
        IV2Pair(v2Pair).swap(out0, out1, address(this), abi.encodePacked(uint8(1)));
    }

    /// @notice V2 flash swap callback - we have the borrowed tokens, now arb and repay
    function uniswapV2Call(address, uint, uint, bytes calldata) external {
        _doArb();
    }

    // Different DEXes use different callback names
    function hook(address, uint, uint, bytes calldata) external {
        _doArb();
    }

    function BiswapCall(address, uint, uint, bytes calldata) external {
        _doArb();
    }

    function _doArb() internal {
        uint256 borrowed = IERC20(_tokenBorrow).balanceOf(address(this));

        // Sell borrowed tokens on the other pool for _tokenPay
        if (_sellV3) {
            // Sell on V3
            IERC20(_tokenBorrow).approve(_sellPool, borrowed);
            bool zf = _tokenBorrow < _tokenPay;
            IV3Pool(_sellPool).swap(
                address(this), zf, int256(borrowed),
                zf ? MIN_SQRT + 1 : MAX_SQRT - 1, ""
            );
        } else {
            // Sell on V2
            _sellV2(_sellPool, _tokenBorrow, _tokenPay, borrowed);
        }

        // Repay the flash swap with _tokenPay
        uint256 payBal = IERC20(_tokenPay).balanceOf(address(this));
        require(payBal >= _repayAmount, "not enough to repay");
        IERC20(_tokenPay).transfer(msg.sender, _repayAmount);

        // Remaining _tokenPay is profit - leave in contract
    }

    // V3 callback for the sell leg
    function uniswapV3SwapCallback(int256 a0, int256 a1, bytes calldata) external {
        if (a0 > 0) IERC20(IV3Pool(msg.sender).token0()).transfer(msg.sender, uint256(a0));
        if (a1 > 0) IERC20(IV3Pool(msg.sender).token1()).transfer(msg.sender, uint256(a1));
    }
    function pancakeV3SwapCallback(int256 a0, int256 a1, bytes calldata) external {
        if (a0 > 0) IERC20(IV3Pool(msg.sender).token0()).transfer(msg.sender, uint256(a0));
        if (a1 > 0) IERC20(IV3Pool(msg.sender).token1()).transfer(msg.sender, uint256(a1));
    }
    function algebraSwapCallback(int256 a0, int256 a1, bytes calldata) external {
        if (a0 > 0) IERC20(IV3Pool(msg.sender).token0()).transfer(msg.sender, uint256(a0));
        if (a1 > 0) IERC20(IV3Pool(msg.sender).token1()).transfer(msg.sender, uint256(a1));
    }

    function _sellV2(address pair, address tIn, address tOut, uint256 amt) internal {
        IERC20(tIn).transfer(pair, amt);
        (uint112 r0, uint112 r1,) = IV2Pair(pair).getReserves();
        address t0 = IV2Pair(pair).token0();
        (uint256 rIn, uint256 rOut) = tIn == t0
            ? (uint256(r0), uint256(r1))
            : (uint256(r1), uint256(r0));
        uint256 aFee = amt * 997;
        uint256 aOut = (aFee * rOut) / (rIn * 1000 + aFee);
        (uint256 o0, uint256 o1) = tIn == t0 ? (uint256(0), aOut) : (aOut, uint256(0));
        IV2Pair(pair).swap(o0, o1, address(this), "");
    }

    function withdraw(address token) external {
        require(msg.sender == owner);
        IERC20(token).transfer(owner, IERC20(token).balanceOf(address(this)));
    }
    function withdrawETH() external {
        require(msg.sender == owner);
        (bool ok,) = owner.call{value: address(this).balance}("");
        require(ok);
    }
    receive() external payable {}
}
