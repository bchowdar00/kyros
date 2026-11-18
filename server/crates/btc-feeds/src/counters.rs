//! Shared bounded-counter arithmetic for feed accounting.

/// Adds `increment` to `value`, mapping overflow to a caller-chosen error.
pub(crate) fn checked_add<E>(
    value: u64,
    increment: u64,
    on_overflow: impl FnOnce() -> E,
) -> Result<u64, E> {
    value.checked_add(increment).ok_or_else(on_overflow)
}
