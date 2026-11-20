// SPDX-License-Identifier: MIT
pragma solidity 0.8.38;

/// @title StrategyWallets
/// @notice Kyros runs trading strategies with pretend money. Each visitor has one wallet. Every
///         strategy they start gets a sub-wallet under that wallet, holding only the budget moved
///         into it. Each trading round's result is applied to the sub-wallet. When the strategy is
///         stopped the sub-wallet is closed and what is left goes back to the wallet. Amounts are
///         pretend-money cents.
/// @dev The contract holds no token, has no payable function, no owner and no upgrade path. The
///      single writer is the recorder address fixed at construction; only it can write. All
///      arithmetic is Solidity 0.8 checked arithmetic. The int64/uint64 settlement sum is widened
///      to int256 and checked at both ends before it is narrowed, and a wallet's balance is a
///      uint128 so that returning a sub-wallet's uint64 balance to it can never overflow.
contract StrategyWallets {
    /// @notice The only address allowed to write. Set once at construction.
    address public immutable recorder;

    /// @dev A visitor's wallet. `exists` distinguishes an opened wallet from the zero default.
    struct Wallet {
        bool exists;
        uint128 balanceCents;
    }

    /// @dev A strategy's sub-wallet. `exists` is set the moment an id is used and never cleared, so
    ///      a closed id can never be reopened. `closed` marks a returned sub-wallet.
    struct SubWallet {
        bool exists;
        bool closed;
        address owner;
        uint64 budgetCents;
        uint64 balanceCents;
    }

    /// @dev owner => wallet.
    mapping(address => Wallet) private _wallets;
    /// @dev deploymentId => sub-wallet.
    mapping(bytes32 => SubWallet) private _subWallets;
    /// @dev owner => the deploymentIds opened under that owner, in the order opened.
    mapping(address => bytes32[]) private _subWalletsOf;
    /// @dev deploymentId => roundId => whether that round has been settled.
    mapping(bytes32 => mapping(uint64 => bool)) private _settled;

    /// @notice A wallet was opened for `owner` with `balanceCents`.
    event WalletOpened(address indexed owner, uint128 balanceCents);
    /// @notice A sub-wallet was opened, funded with `budgetCents` moved out of the owner's wallet.
    event SubWalletOpened(bytes32 indexed deploymentId, address indexed owner, uint64 budgetCents);
    /// @notice A round was settled onto a sub-wallet, leaving it at `balanceCents`.
    event SubWalletSettled(bytes32 indexed deploymentId, uint64 indexed roundId, int64 pnlCents, uint64 balanceCents);
    /// @notice A sub-wallet was closed and `returnedCents` went back to the owner's wallet.
    event SubWalletReturned(bytes32 indexed deploymentId, address indexed owner, uint64 returnedCents);

    /// @notice Caller is not the recorder.
    error NotRecorder();
    /// @notice The zero address can be neither the recorder nor a wallet's owner.
    error ZeroAddress();
    /// @notice This owner already has a wallet.
    error WalletExists(address owner);
    /// @notice This owner has no wallet.
    error NoWallet(address owner);
    /// @notice This deploymentId has already been used for a sub-wallet.
    error SubWalletExists(bytes32 deploymentId);
    /// @notice No sub-wallet exists for this deploymentId.
    error NoSubWallet(bytes32 deploymentId);
    /// @notice This sub-wallet has been closed.
    error SubWalletClosed(bytes32 deploymentId);
    /// @notice The wallet holds less than the requested budget.
    error InsufficientBalance(address owner, uint128 balanceCents, uint64 budgetCents);
    /// @notice This (deploymentId, roundId) has already been settled.
    error RoundAlreadySettled(bytes32 deploymentId, uint64 roundId);
    /// @notice Applying this PnL would take the sub-wallet balance below zero.
    error Overdrawn(bytes32 deploymentId, uint64 balanceCents, int64 pnlCents);
    /// @notice Applying this PnL would take the sub-wallet balance past the largest it can hold.
    error BalanceTooLarge(bytes32 deploymentId, uint64 balanceCents, int64 pnlCents);

    constructor(address recorder_) {
        if (recorder_ == address(0)) revert ZeroAddress();
        recorder = recorder_;
    }

    modifier onlyRecorder() {
        if (msg.sender != recorder) revert NotRecorder();
        _;
    }

    /// @notice Open the single wallet for `owner`, funded with `balanceCents`. Once per owner.
    function openWallet(address owner, uint64 balanceCents) external onlyRecorder {
        if (owner == address(0)) revert ZeroAddress();
        if (_wallets[owner].exists) revert WalletExists(owner);
        _wallets[owner] = Wallet({exists: true, balanceCents: balanceCents});
        emit WalletOpened(owner, balanceCents);
    }

    /// @notice Open a sub-wallet for `owner`, moving `budgetCents` out of the wallet into it. The
    ///         `deploymentId` must never have been used before. A zero budget is allowed.
    function openSubWallet(bytes32 deploymentId, address owner, uint64 budgetCents) external onlyRecorder {
        Wallet storage wallet = _wallets[owner];
        if (!wallet.exists) revert NoWallet(owner);
        if (_subWallets[deploymentId].exists) revert SubWalletExists(deploymentId);
        if (wallet.balanceCents < budgetCents) revert InsufficientBalance(owner, wallet.balanceCents, budgetCents);

        wallet.balanceCents -= budgetCents;
        _subWallets[deploymentId] = SubWallet({
            exists: true,
            closed: false,
            owner: owner,
            budgetCents: budgetCents,
            balanceCents: budgetCents
        });
        _subWalletsOf[owner].push(deploymentId);
        emit SubWalletOpened(deploymentId, owner, budgetCents);
    }

    /// @notice Apply a round's PnL to a sub-wallet's balance. Once per (deploymentId, roundId). The
    ///         owner's wallet balance is untouched. Reverts if the result would be negative or
    ///         more than a sub-wallet can hold.
    function settle(bytes32 deploymentId, uint64 roundId, int64 pnlCents) external onlyRecorder {
        SubWallet storage sub = _subWallets[deploymentId];
        if (!sub.exists) revert NoSubWallet(deploymentId);
        if (sub.closed) revert SubWalletClosed(deploymentId);
        if (_settled[deploymentId][roundId]) revert RoundAlreadySettled(deploymentId, roundId);

        // Widen to int256 so the sum is checked before narrowing back to uint64.
        int256 next = int256(uint256(sub.balanceCents)) + int256(pnlCents);
        if (next < 0) revert Overdrawn(deploymentId, sub.balanceCents, pnlCents);
        if (next > int256(uint256(type(uint64).max))) {
            revert BalanceTooLarge(deploymentId, sub.balanceCents, pnlCents);
        }

        uint64 balanceCents = uint64(uint256(next));
        sub.balanceCents = balanceCents;
        _settled[deploymentId][roundId] = true;
        emit SubWalletSettled(deploymentId, roundId, pnlCents, balanceCents);
    }

    /// @notice Close a sub-wallet: return its balance to the owner's wallet and mark it closed.
    ///         Always possible for an open sub-wallet: the wallet's uint128 has room for every
    ///         uint64 balance that could come back to it.
    function closeSubWallet(bytes32 deploymentId) external onlyRecorder {
        SubWallet storage sub = _subWallets[deploymentId];
        if (!sub.exists) revert NoSubWallet(deploymentId);
        if (sub.closed) revert SubWalletClosed(deploymentId);

        uint64 returnedCents = sub.balanceCents;
        _wallets[sub.owner].balanceCents += returnedCents;
        sub.balanceCents = 0;
        sub.closed = true;
        emit SubWalletReturned(deploymentId, sub.owner, returnedCents);
    }

    /// @notice Read back a wallet: whether it exists and its current balance.
    function walletOf(address owner) external view returns (bool exists, uint128 balanceCents) {
        Wallet storage wallet = _wallets[owner];
        return (wallet.exists, wallet.balanceCents);
    }

    /// @notice Read back a sub-wallet: whether the id was ever opened, its owner, the budget it
    ///         opened with, its current balance, and whether it has been closed.
    function subWalletOf(bytes32 deploymentId)
        external
        view
        returns (bool exists, address owner, uint64 budgetCents, uint64 balanceCents, bool closed)
    {
        SubWallet storage sub = _subWallets[deploymentId];
        return (sub.exists, sub.owner, sub.budgetCents, sub.balanceCents, sub.closed);
    }

    /// @notice The deploymentIds opened under `owner`, in the order they were opened.
    function subWalletsOf(address owner) external view returns (bytes32[] memory) {
        return _subWalletsOf[owner];
    }

    /// @notice Whether a (deploymentId, roundId) has been settled.
    function settled(bytes32 deploymentId, uint64 roundId) external view returns (bool) {
        return _settled[deploymentId][roundId];
    }
}
