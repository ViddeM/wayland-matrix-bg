# Installing wayland-matrix-bg

## Requirements

- A Wayland compositor with wlr-layer-shell: niri, sway, Hyprland, river, KDE Plasma, etc. (not GNOME).
- OpenGL ES 3.0 through EGL: Mesa on AMD/Intel, or NVIDIA's driver.
- Rust 1.86 or newer (use [rustup](https://rustup.rs) if your distro's is older), a C toolchain,
  pkg-config, and the EGL and Wayland development files:

  ```sh
  # Debian/Ubuntu
  sudo apt install build-essential pkg-config libegl-dev libwayland-dev
  # Fedora
  sudo dnf install gcc pkgconf 'pkgconfig(egl)' 'pkgconfig(wayland-client)' 'pkgconfig(wayland-egl)'
  # Arch
  sudo pacman -S --needed base-devel wayland libglvnd
  ```

- fontconfig and a font with half-width katakana, such as Noto Sans CJK (`fonts-noto-cjk`,
  `google-noto-sans-cjk-fonts`, `noto-fonts-cjk`). Or point `--font` at any font that has them.

## Build and install

```sh
cargo install --locked --path .    # installs ~/.cargo/bin/wayland-matrix-bg
wayland-matrix-bg                  # test run; Ctrl-C to stop
```

## Autostart

**niri with systemd** (the default `niri-session` setup):

```sh
install -Dm644 resources/wayland-matrix-bg.service ~/.config/systemd/user/wayland-matrix-bg.service
systemctl --user daemon-reload
systemctl --user add-wants niri.service wayland-matrix-bg.service
systemctl --user start wayland-matrix-bg
```

**Other systemd sessions:** install the unit as above, then `systemctl --user enable --now wayland-matrix-bg`.

**niri without systemd:** add the `spawn-sh-at-startup` line from `resources/niri.kdl` to
`~/.config/niri/config.kdl`.

**sway:** `exec ~/.cargo/bin/wayland-matrix-bg` in `~/.config/sway/config`.

**Hyprland:** `exec-once = ~/.cargo/bin/wayland-matrix-bg` in `~/.config/hypr/hyprland.conf`.

## Overview backdrop (niri)

To see the rain behind workspaces in the overview, copy the `layer-rule` from `resources/niri.kdl`
into `~/.config/niri/config.kdl`.

## Replace your current wallpaper tool

Run only one background client. If swaybg, swww, hyprpaper or similar is also running, which one
ends up on top is undefined. Stop it and remove it from autostart. For niri's documented swaybg setup:

```sh
systemctl --user disable --now swaybg    # also removes the niri.service.wants symlink
```

For `spawn-at-startup`, `exec` or `exec-once` lines, delete them from the compositor config.

## Options

| Flag            | Default                          | Meaning                                                  |
|-----------------|----------------------------------|----------------------------------------------------------|
| `--fps`         | `30`                             | Frames per second per output                             |
| `--color`       | `#00ff41`                        | Rain colour, `#rrggbb`                                   |
| `--cell-size`   | `18`                             | Glyph cell height in logical pixels (4–256)              |
| `--speed`       | `1.0`                            | Fall speed multiplier                                    |
| `--font`        | fc-match `Noto Sans Mono CJK JP` | Font file, `path[:face-index]`                           |
| `--namespace`   | `wallpaper`                      | Layer-shell namespace, for layer rules                   |
| `--clock`       | off                              | Local time (24-hour `HH:MM`) drawn by the rain, centred  |
| `--clock-size`  | `0.2`                            | Clock digit height as a fraction of the output height    |
| `--clock-blink` | off                              | Blink the clock's colon once a second                    |

With systemd, put flags on the unit's `ExecStart=` line, then:

```sh
systemctl --user daemon-reload && systemctl --user restart wayland-matrix-bg
```

## Notes

- **Performance:** a few percent of one CPU core at 30 fps. GPU cost is about one full-screen fill
  per output per frame. `--fps` scales both. On NVIDIA it sets `__GL_YIELD=USLEEP` (unless you
  set it yourself), because the driver's default busy-wait uses much more CPU.
- **Multi-GPU:** it should render on the GPU your compositor uses. If it picks the wrong one,
  override it with `Environment=` in the unit (examples are in the file):
  - NVIDIA plus Mesa: `__EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json`
    (or `10_nvidia.json`).
  - Two Mesa GPUs: `DRI_PRIME=1`.

  Don't force Mesa while the compositor renders on NVIDIA. Mesa then falls back to llvmpipe
  (software rendering), which uses several CPU cores.
- **Outputs:** each output gets its own rain. Hotplug and fractional scaling are handled
  automatically.

## Troubleshooting

- Logs: `journalctl --user -u wayland-matrix-bg -b`, or run `wayland-matrix-bg` in a terminal.
- `wlr-layer-shell: ...`: the compositor doesn't support layer-shell.
- `... lacks half-width katakana` or `fc-match found no font`: list suitable fonts with

  ```sh
  fc-list -f '%{file}:%{index}  %{family[0]}\n' ':charset=ff71'
  ```

  Then install a CJK font (see Requirements) or pass one of the listed `path:index` values to `--font`.
- No rain, but no error either: another wallpaper client is probably on top (see above).

## Uninstall

```sh
systemctl --user disable --now wayland-matrix-bg    # removes add-wants and enable symlinks
rm ~/.config/systemd/user/wayland-matrix-bg.service
systemctl --user daemon-reload
cargo uninstall wayland-matrix-bg
```

Remove any autostart lines you added and re-enable your previous wallpaper tool. The niri
`layer-rule` matches swaybg's namespace too, so it can stay.
