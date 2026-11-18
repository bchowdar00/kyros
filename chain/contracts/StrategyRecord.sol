// SPDX-License-Identifier: MIT
pragma solidity 0.8.38;

/// @title StrategyRecord
/// @notice An append-only, on-chain ledger of Kyros strategy deployments and the rounds they
///         settle. For each deployment it records the opening (its program hash and budget) once,
///         and for each (deployment, round) it records the settlement (won / PnL / journal hash)
///         once. It deliberately stores **no market price of any kind** — only the facts a Kyros
///         deployment commits to and the outcome its own journal attests.
/// @dev No owner, no upgradeability, no payable paths, no external calls. The single writer is the
///      recorder address fixed at construction. Anyone can read a record back with `opened` /
///      `sealed` without scanning logs.
contract StrategyRecord {
    /// @notice The only address allowed to write records. Set once at construction.
    address public immutable recorder;

    /// @dev deploymentId => whether `open` has been recorded for it.
    mapping(bytes32 => bool) private _opened;
    /// @dev deploymentId => roundId => whether `seal` has been recorded for that round.
    mapping(bytes32 => mapping(uint64 => bool)) private _sealed;

    /// @notice A deployment's opening was recorded.
    event Opened(bytes32 indexed deploymentId, bytes32 programHash, uint64 budgetCents);
    /// @notice A deployment's round settlement was recorded.
    event Sealed(bytes32 indexed deploymentId, uint64 indexed roundId, bool won, int64 pnlCents, bytes32 journalHash);

    /// @notice Caller is not the recorder.
    error NotRecorder();
    /// @notice This deployment has already been opened.
    error AlreadyOpened();
    /// @notice This deployment has not been opened.
    error NotOpened();
    /// @notice This (deployment, round) has already been sealed.
    error AlreadySealed();

    constructor(address recorder_) {
        recorder = recorder_;
    }

    modifier onlyRecorder() {
        if (msg.sender != recorder) revert NotRecorder();
        _;
    }

    /// @notice Record a deployment's opening. Once per `deploymentId`.
    function open(bytes32 deploymentId, bytes32 programHash, uint64 budgetCents) external onlyRecorder {
        if (_opened[deploymentId]) revert AlreadyOpened();
        _opened[deploymentId] = true;
        emit Opened(deploymentId, programHash, budgetCents);
    }

    /// @notice Record a deployment round's settlement. Once per (`deploymentId`, `roundId`), and
    ///         only after the deployment has been opened.
    function seal(bytes32 deploymentId, uint64 roundId, bool won, int64 pnlCents, bytes32 journalHash)
        external
        onlyRecorder
    {
        if (!_opened[deploymentId]) revert NotOpened();
        if (_sealed[deploymentId][roundId]) revert AlreadySealed();
        _sealed[deploymentId][roundId] = true;
        emit Sealed(deploymentId, roundId, won, pnlCents, journalHash);
    }

    /// @notice Whether a deployment's opening has been recorded.
    function opened(bytes32 deploymentId) external view returns (bool) {
        return _opened[deploymentId];
    }

    /// @notice Whether a deployment round's settlement has been recorded.
    /// @dev Named `isSealed`, not `sealed`: `sealed` is a reserved keyword in Solidity and will
    ///      not compile as an identifier. `opened` is not reserved, so it keeps its spec name.
    function isSealed(bytes32 deploymentId, uint64 roundId) external view returns (bool) {
        return _sealed[deploymentId][roundId];
    }
}
