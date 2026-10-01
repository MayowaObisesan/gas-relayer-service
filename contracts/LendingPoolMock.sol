// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @title LendingPoolMock — the pool a keeper watches
/// @notice A deliberately tiny over-collateralised lending pool, built so that the Week 10
///         "liquidation watchdog" has something real to act on.
///
/// The economics are the whole point: `healthFactor = collateral * 1e18 / debt`, so a position is
/// liquidatable exactly when `healthFactor < 1.0`. Two ways to get there, both used in the lab:
///
///   1. `deposit(collateral, debt)` with too little collateral — the user opens underwater.
///   2. `deposit(...)` healthy, then `withdraw(collateral)` until the ratio drops below 1.0 —
///      the case a *watcher* must notice, because nothing about the second call looks alarming
///      in isolation. This is the one the `Deposit`/`Withdraw` log filter exists for.
///
/// `checkUpkeep()` returns `(bool upkeepNeeded, bytes payload)` and `performUpkeep(payload)`
/// consumes it, so the keeper is a dumb executor of a decision the *contract* makes. A keeper that
/// re-derived the condition off-chain would be able to liquidate a healthy position the moment its
/// own copy of the state went stale; this shape makes that class of bug unrepresentable.
///
/// Readable source for `src/keeper/mock.rs`, which embeds the compiled creation bytecode so the
/// demo runs on a bare `anvil` with no toolchain. `scripts/deploy_pool.sh` recompiles and
/// redeploys this exact file when you would rather see the compiler do the work.
contract LendingPoolMock {
    // ------------------------------------------------------------------------------------------
    // Types & immutables
    // ------------------------------------------------------------------------------------------

    /// A user's position. `collateral` is denominated in 1e18-scaled units so the health factor
    /// is a plain integer division with no precision surprises.
    struct Position {
        uint128 collateral;
        uint128 debt;
        /// Set once the position has been liquidated. Without this, `checkUpkeep` would keep
        /// reporting the same unhealthy user and the keeper would loop paying gas forever.
        bool liquidated;
    }

    /// One collateral unit per one debt unit is the liquidation line, expressed at 1e18 so that
    /// `healthFactor() == 1e18` means "exactly at the line".
    uint256 public constant HEALTH_FACTOR_ONE = 1e18;

    /// 1 wei of debt worth 2% of the seized collateral: enough to make the bonus real, small
    /// enough that the numbers on the projector stay readable.
    uint256 public constant LIQUIDATION_BONUS_BPS = 200;

    /// The lab's fixed "victim": an address nobody controls a key for, so a student never has to
    /// pass one on the command line and the keeper can never be tempted to liquidate the account
    /// it is signing with.
    address public constant VICTIM = address(0x000000000000000000000000000000000000bEEF);

    address public immutable owner;

    // ------------------------------------------------------------------------------------------
    // Storage
    // ------------------------------------------------------------------------------------------

    mapping(address user => Position position) public positions;

    /// Incremented on every liquidation, so `performUpkeep` is idempotent-by-construction for a
    /// given payload: replaying the same payload cannot liquidate twice.
    uint256 public liquidationCount;

    // ------------------------------------------------------------------------------------------
    // Events — the watcher's trigger set
    // ------------------------------------------------------------------------------------------

    /// A position was opened or its debt was topped up.
    event Deposit(address indexed user, uint256 collateral, uint256 debt);

    /// Collateral left a position. The call that most often pushes a user under water.
    event Withdraw(address indexed user, uint256 collateral, uint256 debt);

    /// A position was below the health line and was seized.
    event Liquidated(address indexed user, address indexed liquidator, uint256 collateral, uint256 bonus);

    /// Announced by `performUpkeep` so an operator can see the keeper's call land.
    event UpkeepPerformed(address indexed user, bytes payload);

    // ------------------------------------------------------------------------------------------
    // Errors
    // ------------------------------------------------------------------------------------------

    error NothingToLiquidate(address user);
    error AlreadyLiquidated(address user);
    error SelfLiquidation(address user);
    error ZeroAmount();

    constructor() {
        owner = msg.sender;
    }


    // ------------------------------------------------------------------------------------------
    // User actions
    // ------------------------------------------------------------------------------------------

    /// Open or adjust a position. Deliberately does **not** require the position to be healthy:
    /// a pool that refused to open an underwater position could never be liquidated, and the
    /// whole watchdog would be untestable.
    function deposit(uint256 collateral, uint256 debt) external {
        if (collateral == 0 && debt == 0) revert ZeroAmount();
        Position storage p = positions[msg.sender];
        p.collateral += uint128(collateral);
        p.debt += uint128(debt);
        emit Deposit(msg.sender, collateral, debt);
    }

    /// Withdraw collateral, shrinking the health factor. Withdrawing *more* than exists reverts.
    function withdraw(uint256 collateral) external {
        if (collateral == 0) revert ZeroAmount();
        Position storage p = positions[msg.sender];
        if (p.collateral < collateral) revert NothingToLiquidate(msg.sender);
        p.collateral -= uint128(collateral);
        emit Withdraw(msg.sender, collateral, p.debt);
    }

    /// Withdraw collateral **from the victim position**, shrinking its health factor.
    ///
    /// Separate from `withdraw` because that one acts on `msg.sender`, and the lab's victim is a
    /// fixed address nobody holds a key for. Merging the two would make the demo impossible: the
    /// only way to arm the victim is `armVictim`, and the only way to un-arm it would be a
    /// `withdraw` sent from an account that does not exist.
    ///
    /// This exists so the lab can show the case a *watcher* must catch: a position that was
    /// healthy, followed by a call that looks completely ordinary in isolation, and a health factor
    /// that has quietly fallen below 1.0.
    function withdrawVictim(uint256 collateral) external {
        if (collateral == 0) revert ZeroAmount();
        Position storage p = positions[VICTIM];
        if (p.collateral < collateral) revert NothingToLiquidate(VICTIM);
        p.collateral -= uint128(collateral);
        emit Withdraw(VICTIM, collateral, p.debt);
    }

    /// Repay all debt, which always restores a position to healthy (collateral ≥ debt, no bonus
    /// owed). The "do nothing, wait" alternative to being liquidated.
    function repay(uint256 debt) external {
        Position storage p = positions[msg.sender];
        if (debt == 0) revert ZeroAmount();
        if (p.debt < debt) revert NothingToLiquidate(msg.sender);
        p.debt -= uint128(debt);
        emit Deposit(msg.sender, 0, 0);
    }

    // ------------------------------------------------------------------------------------------
    // Views
    // ------------------------------------------------------------------------------------------

    /// `collateral * 1e18 / debt`, or `type(uint256).max` when there is no debt. A position with
    /// zero debt is infinitely healthy; the sentinel keeps the keeper from dividing by zero and
    /// keeps the comparison below monotonic.
    function healthFactor(address user) public view returns (uint256) {
        Position storage p = positions[user];
        if (p.debt == 0) return type(uint256).max;
        if (p.liquidated) return 0;
        return (uint256(p.collateral) * HEALTH_FACTOR_ONE) / uint256(p.debt);
    }

    /// `true` when the position is liquidatable. A `view` function the keeper can call for free.
    function isLiquidatable(address user) public view returns (bool) {
        Position storage p = positions[user];
        if (p.liquidated) return false;
        if (p.debt == 0) return false;
        return (uint256(p.collateral) * HEALTH_FACTOR_ONE) / uint256(p.debt) < HEALTH_FACTOR_ONE;
    }


    /// The lab's fixed "victim", as a function so a `cast call` can read it like any other view.
    function victim() external view returns (address) {
        return VICTIM;
    }

    // ------------------------------------------------------------------------------------------
    // The upkeep interface — the only two functions the keeper is allowed to call
    // ------------------------------------------------------------------------------------------

    /// The condition check. Returns the *payload* it wants `performUpkeep` to consume, so the
    /// keeper never has to decide anything on the contract's behalf.
    ///
    /// The contract scans a fixed roster rather than maintaining a list of unhealthy users: a
    /// real pool keeps such a list, and that is where a keeper's "which accounts do I even
    /// watch?" bug lives. Two well-known addresses is the whole roster here.
    function checkUpkeep() external view returns (bool upkeepNeeded, bytes memory payload) {
        // `VICTIM` rather than `victim()`: an `external` function has no internal symbol to
        // call, and going through `this.victim()` would turn a constant read into a CALL.
        address[2] memory roster = [address(this), VICTIM];

        for (uint256 i = 0; i < roster.length; ++i) {
            if (isLiquidatable(roster[i])) {
                return (true, abi.encode(roster[i]));
            }
        }
        return (false, "");
    }

    /// The action. Refuses unless the payload's account is *still* liquidatable right now — the
    /// on-chain half of the TOCTOU window the off-chain keeper cannot close. A keeper that raced
    /// a competing liquidator lands here, reverts, and (if the keeper was paying attention) learns
    /// that its own dry run was optimistic.
    function performUpkeep(bytes calldata payload) external returns (address user) {
        if (payload.length != 32) revert NothingToLiquidate(address(0));
        user = abi.decode(payload, (address));
        Position storage p = positions[user];
        if (p.liquidated) revert AlreadyLiquidated(user);
        if (!isLiquidatable(user)) revert NothingToLiquidate(user);
        if (user == msg.sender) revert SelfLiquidation(user);

        uint256 collateral = p.collateral;
        uint256 bonus = (collateral * LIQUIDATION_BONUS_BPS) / 10_000;

        p.collateral = 0;
        p.debt = 0;
        p.liquidated = true;
        liquidationCount += 1;

        emit Liquidated(user, msg.sender, collateral, bonus);
        emit UpkeepPerformed(user, payload);
    }

    // ------------------------------------------------------------------------------------------
    // Test convenience
    // ------------------------------------------------------------------------------------------

    /// Open the lab's "about to be liquidated" position for the fixed victim address. Callable by
    /// anyone; it only writes to `VICTIM`, never to the caller's own position.
    ///
    /// Clears the `liquidated` flag as well as writing the numbers. That is not a convenience: a
    /// position that has already been seized is permanently un-liquidatable *by design* (so the
    /// keeper cannot liquidate twice and bill the pool for it), which means a re-arm that only
    /// wrote `collateral` and `debt` would leave the victim looking healthy forever.
    ///
    /// The first version of this function had exactly that bug, and the symptom was nasty:
    /// `checkUpkeep()` returned `false` immediately after arming, which reads as "the keeper is
    /// broken" rather than "the mock is wrong". Found by running the demo twice.
    function armVictim(uint256 collateral, uint256 debt) external {
        Position storage p = positions[VICTIM];
        p.collateral = uint128(collateral);
        p.debt = uint128(debt);
        p.liquidated = false;
        emit Deposit(VICTIM, collateral, debt);
    }

    /// Owner-only. Clears the position so a demo can be re-armed without a redeploy.
    function reset(address user) external {
        require(msg.sender == owner, "not owner");
        delete positions[user];
    }
}
