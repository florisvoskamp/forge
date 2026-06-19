# Feature: OS-level shell sandbox (Linux Landlock)

> Status: **SHIPPED**. Opt-in, default off.

When `[shell] sandbox = true`, every `shell` tool command runs under a kernel-enforced
[Landlock](https://docs.kernel.org/userspace-api/landlock.html) ruleset that confines filesystem
**writes** to the workspace (the command's cwd), the system temp dir, and any extra paths in
`sandbox_writable`. Reads + execute stay broad so normal tooling (compilers, interpreters, package
managers) still works. This is a real, kernel-enforced boundary — unlike the model-side permission
broker, the process *cannot* write outside the allowed set regardless of what the model decides.

```toml
[shell]
sandbox = true
sandbox_writable = ["/home/me/.cargo"]   # extra writable dirs beyond cwd + temp
```

## How it works

- The sandbox is applied via `pre_exec` on the spawned shell process: in the forked child, just
  before `exec`, Forge installs the Landlock ruleset (read+execute on `/`, read+write on the
  writable set) with `CompatLevel::BestEffort` so partial-ABI kernels still get partial protection.
- Kernel support is probed **once in the parent** before spawning. If Landlock is unavailable
  (old kernel, non-Linux), Forge logs a one-time warning and runs the command **unconfined** —
  the sandbox never hard-fails or blocks a command.
- **Linux-only.** On macOS/Windows the whole path is a compile-time no-op.

## Scope + limits

- Confines **filesystem writes**, not network (network egress restriction via Landlock ABI v4 is a
  possible follow-up). Pair with the permission broker + denylist for command-level control.
- Applies to the in-process `shell` tool on the primary session path. Default off so existing
  behaviour is byte-for-byte unchanged.

Verified on Linux 6.x/7.x (`CONFIG_SECURITY_LANDLOCK=y`): a write inside cwd succeeds, a write to
`/etc/...` is denied with `Permission denied`.
