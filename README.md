# vvland

Run, stream, or automate one isolated Wayland app or compositor.

`vvland` starts a host-supplied Wayland compositor (Weston or Sway), captures its output,
encodes H.264/Opus, and streams the session through the private Vivid endpoint inherited
from Vivido or `vvssh`. It can also keep the desktop in an owner-only headless daemon and drive it
through the local `vvland msg` protocol without a presenter or Vivid credentials.

```sh
vvland --doctor                      # check the host (both compositors)
vvland --compositor sway             # stream an isolated Sway desktop
vvland --app thunar                  # run one application, alone, full-output
vvland --app google-chrome -- --user-data-dir=/tmp/x
vvland serve --session work          # start a detached headless desktop
vvland --session work                # attach; starts the desktop if absent
vvland msg -t work inspect           # inspect and automate it
vvland msg -t work screenshot > shot.png
```

`--compositor auto|weston|sway` selects the session; `--app google-chrome|thunar` starts a
headless compositor running exactly one application.

In session mode, `C-b d` detaches while leaving the desktop alive, `C-b q` shuts the daemon down,
and `SIGINT`, `SIGHUP`, or `SIGTERM` detach. A later `vvland --session work` reconnects without
restarting the compositor, capture, input devices, audio sink, or applications. Only one presenter
may be attached; pass `--replace` to disconnect it deliberately. The daemon's geometry is fixed at
creation, while `--bitrate`, `--fps`, `--desktop-target`, and `--secure-input` may vary per attach.

A vvland control socket can type into the desktop, click anything on it, launch programs in it,
and read its screen. **Possession of the socket is equivalent to shell access as the owning
user.** It is 0600 in a 0700 directory and peer-credential checked on both ends, and vvland never
exposes it over a network. The peer check defeats a relay run as a *different* user; it does not
and cannot defeat one run as the same user. Do not relay it, do not `socat` it, and do not run a
session as a user whose desktop you would not hand over.

See `docs/vvland/user-guide.md` for the full guide, including which applications work in
single-app mode and why. The stable automation contract is
`docs/vvland/control-protocol.md`; `docs/vvland-plan.md` records the consolidation of `veston` and
`vvsway`. Repeatable Chrome streaming checks and latency diagnostics are in
`docs/vvland/performance.md`.

## Usage

Compositor:

    vvland --compositor weston
    vvland --compositor sway -- weston-simple-egl

Compositor with app:

    vvland --compositor sway -- weston-simple-egl
    vvland --weston weston --drm-device=/dev/dri/card0 --drm-output=DP-1  -- weston-simple-egl
