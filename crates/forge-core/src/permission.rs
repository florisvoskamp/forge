//! The permission broker (ADR-0008): the single chokepoint that decides whether a tool
//! with side effects may run. Resolves a [`PermissionDecision`] from the session's mode.
//! Per-tool/per-project allow-ask-deny rules layer on top of this (planned enhancement);
//! the precedence is: an explicit `deny` wins, then `plan` mode denies, then the mode
//! decides.

use forge_types::{PermissionDecision, PermissionMode, SideEffect};

/// Decide the outcome for a tool with the given side-effect class under `mode`.
pub fn decide(mode: PermissionMode, side_effect: SideEffect) -> PermissionDecision {
    use PermissionDecision::*;

    // Read-only tools never prompt, regardless of mode.
    if side_effect == SideEffect::ReadOnly {
        return Allow;
    }

    match mode {
        // Read-only session: no side effects at all.
        PermissionMode::Plan => Deny,
        // Explicit, deliberate "do anything".
        PermissionMode::Bypass => Allow,
        // Auto-allow edits, still gate shell.
        PermissionMode::AcceptEdits => match side_effect {
            SideEffect::Write => Allow,
            SideEffect::Shell => Ask,
            SideEffect::ReadOnly => Allow,
        },
        // Safe default: confirm any side effect.
        PermissionMode::Default => Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PermissionDecision::*;

    #[test]
    fn read_only_always_allowed() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
            PermissionMode::Plan,
        ] {
            assert_eq!(decide(mode, SideEffect::ReadOnly), Allow);
        }
    }

    #[test]
    fn plan_denies_all_side_effects() {
        assert_eq!(decide(PermissionMode::Plan, SideEffect::Write), Deny);
        assert_eq!(decide(PermissionMode::Plan, SideEffect::Shell), Deny);
    }

    #[test]
    fn accept_edits_allows_write_asks_shell() {
        assert_eq!(
            decide(PermissionMode::AcceptEdits, SideEffect::Write),
            Allow
        );
        assert_eq!(decide(PermissionMode::AcceptEdits, SideEffect::Shell), Ask);
    }

    #[test]
    fn bypass_allows_everything() {
        assert_eq!(decide(PermissionMode::Bypass, SideEffect::Shell), Allow);
    }

    #[test]
    fn default_asks_for_side_effects() {
        assert_eq!(decide(PermissionMode::Default, SideEffect::Write), Ask);
        assert_eq!(decide(PermissionMode::Default, SideEffect::Shell), Ask);
    }
}
