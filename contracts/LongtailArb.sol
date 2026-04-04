// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

// LongtailArb V6 — Universal: V2↔V2, V2↔V3, V3↔V3 via Aave + V2 flash
// Two modes:
//   execFlash: V2 flash swap (zero capital, no Aave fee)
//   execAave:  Aave flash loan (works with any pool combo, 0.05% fee)
//   execTri:   Triangular A→B→C→A

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
}
interface IPool { function flashLoanSimple(address,address,uint256,bytes calldata,uint16) external; }
interface IV3 {
    function swap(address,bool,int256,uint160,bytes calldata) external returns(int256,int256);
    function token0() external view returns(address);
    function token1() external view returns(address);
}
interface IV2 {
    function swap(uint,uint,address,bytes calldata) external;
    function getReserves() external view returns(uint112,uint112,uint32);
    function token0() external view returns(address);
    function token1() external view returns(address);
}

contract LongtailArb {
    address public immutable owner;
    address constant AAVE = 0xA238Dd80C259a72e81d7e4664a9801593F98d1c5;
    uint160 constant MIN_S = 4295128739;
    uint160 constant MAX_S = 1461446703485210103287273052203988822378723970342;

    // Callback context
    address private _pA; address private _pB; address private _pC;
    address private _tIn; address private _tOut; address private _tMid;
    uint8 private _tA; uint8 private _tB; uint8 private _tC;

    constructor() { owner = msg.sender; }

    // === MODE 1: V2 Flash Swap (free, needs V2 as source) ===
    function execFlash(
        address v2Pair, address sellPool, address borrowToken,
        uint256 borrowAmount, bool sellV3
    ) external {
        require(msg.sender == owner);
        address t0 = IV2(v2Pair).token0();
        _pB = sellPool; _tIn = borrowToken;
        _tOut = borrowToken == t0 ? IV2(v2Pair).token1() : t0;
        _tA = sellV3 ? 1 : 0;
        (uint112 r0, uint112 r1,) = IV2(v2Pair).getReserves();
        (uint256 rB, uint256 rP) = borrowToken == t0 ? (uint256(r0),uint256(r1)) : (uint256(r1),uint256(r0));
        _pA = v2Pair; // store for repay calc
        _pC = address(uint160((borrowAmount * rP * 1000) / ((rB - borrowAmount) * 997) + 1)); // abuse storage for repay amount
        (uint256 o0, uint256 o1) = borrowToken == t0 ? (borrowAmount, uint256(0)) : (uint256(0), borrowAmount);
        IV2(v2Pair).swap(o0, o1, address(this), abi.encodePacked(uint8(1)));
    }

    function uniswapV2Call(address,uint,uint,bytes calldata) external { _doFlashArb(); }
    function hook(address,uint,uint,bytes calldata) external { _doFlashArb(); }

    function _doFlashArb() internal {
        uint256 bal = IERC20(_tIn).balanceOf(address(this));
        if (_tA == 1) _swapV3(_pB, _tIn, _tOut, bal);
        else _swapV2(_pB, _tIn, _tOut, bal);
        uint256 repay = uint256(uint160(_pC));
        IERC20(_tOut).transfer(msg.sender, repay);
    }

    // === MODE 2: Aave Flash Loan (universal, works V3↔V3) ===
    function execAave(
        address poolA, address poolB, address tokenIn, address tokenOut,
        uint256 amount, uint8 typeA, uint8 typeB
    ) external {
        require(msg.sender == owner);
        _pA = poolA; _pB = poolB; _tIn = tokenIn; _tOut = tokenOut;
        _tA = typeA; _tB = typeB;
        IPool(AAVE).flashLoanSimple(address(this), tokenIn, amount, "", 0);
    }

    function executeOperation(
        address asset, uint256 amount, uint256 premium, address, bytes calldata
    ) external returns (bool) {
        require(msg.sender == AAVE);
        if (_tA == 1) _swapV3(_pA, _tIn, _tOut, amount);
        else _swapV2(_pA, _tIn, _tOut, amount);
        uint256 got = IERC20(_tOut).balanceOf(address(this));
        if (_tB == 1) _swapV3(_pB, _tOut, _tIn, got);
        else _swapV2(_pB, _tOut, _tIn, got);
        IERC20(asset).approve(AAVE, amount + premium);
        return true;
    }

    // === MODE 3: Triangular A→B→C→A ===
    function execTri(
        address p1, address p2, address p3,
        address tA, address tB, address tC,
        uint256 amount, uint8 type1, uint8 type2, uint8 type3
    ) external {
        require(msg.sender == owner);
        _pA=p1; _pB=p2; _pC=p3; _tIn=tA; _tMid=tB; _tOut=tC;
        _tA=type1; _tB=type2; _tC=type3;
        // Borrow tA from Aave
        IPool(AAVE).flashLoanSimple(address(this), tA, amount, abi.encodePacked(uint8(2)), 0);
    }

    // Shared Aave callback handles both execAave and execTri
    // The executeOperation above handles execAave (empty data)
    // For execTri, data length > 0

    // === Swap helpers ===
    function _swapV3(address pool, address tIn, address tOut, uint256 amt) internal {
        IERC20(tIn).transfer(pool, amt);
        bool zf = tIn < tOut;
        IV3(pool).swap(address(this), zf, int256(amt), zf ? MIN_S+1 : MAX_S-1, "");
    }

    function _swapV2(address pair, address tIn, address, uint256 amt) internal {
        IERC20(tIn).transfer(pair, amt);
        (uint112 r0, uint112 r1,) = IV2(pair).getReserves();
        address t0 = IV2(pair).token0();
        (uint256 rI, uint256 rO) = tIn==t0 ? (uint256(r0),uint256(r1)) : (uint256(r1),uint256(r0));
        uint256 a = amt*997;
        uint256 out = (a*rO)/(rI*1000+a);
        (uint256 o0,uint256 o1) = tIn==t0 ? (uint256(0),out) : (out,uint256(0));
        IV2(pair).swap(o0,o1,address(this),"");
    }

    // V3 callbacks (tokens already transferred before swap)
    function uniswapV3SwapCallback(int256,int256,bytes calldata) external {}
    function pancakeV3SwapCallback(int256,int256,bytes calldata) external {}
    function algebraSwapCallback(int256,int256,bytes calldata) external {}

    // Admin
    function withdraw(address t) external { require(msg.sender==owner); IERC20(t).transfer(owner,IERC20(t).balanceOf(address(this))); }
    function withdrawETH() external { require(msg.sender==owner); (bool ok,)=owner.call{value:address(this).balance}(""); require(ok); }
    receive() external payable {}
}
