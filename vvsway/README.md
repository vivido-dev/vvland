# Vvsway

> **Deprecated.** `vvsway` is now a thin wrapper around **`vvland`**. `vvsway ARGS` runs
> `vvland --compositor sway ARGS`, with the same flags and the same behavior, so existing
> invocations, scripts, and `vvssh` instructions keep working. New work — and the documentation —
> is in `vvland/`.

Vvsway streamed an isolated headless Sway desktop (Sway 1.9 or newer) into Vivido. That producer
is now one half of `vvland`, which runs either Weston or Sway from a single binary.

```sh
vvsway --doctor                 # identical to: vvland --compositor sway --doctor
vvsway -- weston-terminal       # identical to: vvland --compositor sway -- weston-terminal
```

vvland can do things this wrapper cannot express, including running the other compositor from the
same binary and single-app mode:

```sh
vvland --compositor auto           # probe the host and pick a compositor
vvland --app google-chrome         # run one application, alone, filling the output
```

## Documentation

- [vvland user guide](../docs/vvland/user-guide.md) — the CLI, `--app` mode, environment,
  and which applications work in single-app mode
- [vvland architecture](../docs/vvland/architecture.md)
- [consolidation plan](../archive/docs/vvland-plan.md) — why this crate became a wrapper
- [`docs/vvsway/`](../docs/vvsway/) — this producer's pre-consolidation documentation

## Installing

```sh
cargo install --path .     # installs the `vvsway` wrapper
cargo install --path ../vvland
```

Host requirements are unchanged and are checked by `--doctor`.
