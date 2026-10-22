# portable-pty vendor source

<!-- @author kongweiguang -->

This directory starts from the published `portable-pty` 0.9.0 crate source.

- Upstream repository: <https://github.com/wezterm/wezterm>
- Upstream path: `pty`
- Upstream revision recorded by the crate package: `f8921727a11b9f8b073e8c24821d72fd41283500`
- Package license: MIT; the original `LICENSE.md` is retained unchanged.

The production changes are limited to the Windows ConPTY startup and natural-exit lifecycle:

- `SlavePty::spawn_command_in_job` accepts a borrowed Windows Job handle and fails closed by default.
- ConPTY adds `PROC_THREAD_ATTRIBUTE_JOB_LIST` beside `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` in one `STARTUPINFOEXW` attribute list, then makes one `CreateProcessW` call.
- ConPTY sets `STARTF_USESTDHANDLES` with null standard handles so a redirected parent console does not pass its stdio handles to the child; handle inheritance remains disabled.
- The existing `spawn_command` entry point keeps its original non-Job behavior.
- `MasterPty::release_after_child_exit` is an optional, non-destructive lifecycle hook. The Windows backend resolves `ReleasePseudoConsole` only from the same loaded DLL instance that supplied `CreatePseudoConsole`; a missing symbol reports the capability unavailable. The API's `HRESULT` must be `S_OK` before release is recorded; any other result is returned as an error while the ConPTY owner remains available for retry or explicit cleanup. `ClosePseudoConsole` remains the final resource release and is not used to imitate graceful release.

The caller owns and configures the Job Object, including `KILL_ON_JOB_CLOSE`, and must retain that owner for the child lifetime. This patch neither owns nor closes the supplied handle and never falls back to a post-spawn assignment.
