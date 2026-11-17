# AGENTS.md

Guidance for agents working on sbox. Keep code, identifiers and comments in
English.

## Rules

- Do not change the main repository's Git state. Never stage or commit changes;
  do not switch branches, reset, stash, rebase, or otherwise alter refs or the
  index. Read-only commands such as `git status` and `git diff` are fine.
- Preserve existing user changes. Keep edits within the requested scope.
- Do not use `expect` or `unwrap` in non-test code. Handle the failure or
  return an error instead; tests may panic, but a daemon that panics takes the
  sandbox down with it.
- Do not weaken namespace, capability, `no_new_privs`, or user-namespace-budget
  protections without an explicit product decision. Avoid adding dependencies
  without a clear need.
- Keep responsibilities clear: `linux/` contains syscall and low-level
  wrappers; policy belongs in `sandbox/` or `app/`; For example, configuration
  must not leak into `linux/`.
- Keep behavior with the type that owns the state. Prefer methods for operations
  that inspect or change an object's state; use free functions for stateless
  primitives. Keep functions and names focused and code straightforward to
  follow. Avoid duplicating state or splitting a long method into unrelated
  helpers just to reduce its length.
- When changing configuration or CLI behavior, update the relevant defaults,
  merge logic, and both READMEs. Keep the English and Chinese documentation in
  sync.

### Visibility

Visibility states an audience once, on the item that defines the boundary:

- Give a struct, enum, trait, module, constant or free function the narrowest
  `pub(...)` its callers need.
- Do not repeat a struct's visibility on its fields or inherent methods. Write
  `pub` for a member that belongs to the type's interface and nothing for an
  internal helper. `pub` there means "part of the type's API", because Rust
  already caps access to a value and its members at the type's visibility.
- Keep an explicit `pub(...)` on a member only when it needs a different
  audience than its type.

## Build and verification

The project requires Linux 5.12+, unprivileged user namespaces, and Rust 1.85+.
Useful commands include `cargo build --release`, `cargo test`, `cargo clippy`,
and `cargo fmt`. Integration tests start real sandboxes and require
`unshare -U --map-root-user true`; namespace-dependent unit tests skip when
namespaces are unavailable.

## Critical invariants

- All sandbox entries join the existing namespaces through file descriptors.
  Never recreate namespaces per invocation or find them through a PID.
- Create the first-level user namespace and map root to the caller, then unshare
  `CLONE_NEWPID` before forking. Only the child becomes PID 1.
- Resolve mount destinations inside the guest root with `openat2` and
  `RESOLVE_IN_ROOT`. Plain `*at` operations must use a single path component.
  Keep mount mechanics in `linux/` and mount layout policy in `sandbox/`.
- Keep the host root unreachable after mount setup finishes. Resolve bind
  sources through a descriptor; do not use a live host path after pivoting.
- PID 1 reaps orphaned processes and retires after the last session unless
  configured to persist.
- The name lock is the authority for sandbox ownership. Remove a stale control
  socket only while holding that lock.
- Bump the control protocol version for incompatible changes. Add protocol
  actions in sync across protocol definitions, daemon dispatch, and the client.
- Sandbox level configuration layers are built-in defaults, `global.toml`,
  `sandbox.d/<name>.toml`, then command-line overrides. `bind` and D-Bus rule
  lists append across layers; other values replace by key. Keep
  invocation-dependent defaults in the layer that uses them.
