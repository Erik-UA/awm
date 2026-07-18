# Plan mode via set_permission_mode (verified 2026-07-18)

Sent `set_permission_mode {mode:plan}` to a persistent claude (no -p).

- mode echo from control_response: `plan`
- ExitPlanMode arrived as a can_use_tool gate: `True`

Conclusion: plan mode accepted; ExitPlanMode comes as an approval gate.
