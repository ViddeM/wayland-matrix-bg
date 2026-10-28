# wayland-matrix-bg

Matrix digital rain as a live wallpaper for Wayland compositors with wlr-layer-shell (niri, sway,
Hyprland, river, KDE Plasma). It renders with OpenGL ES through EGL, draws each output separately,
and uses a few percent of one CPU core at 30 fps.

```sh
cargo install --locked --path .
wayland-matrix-bg --color '#00ff41' --fps 30
```

See [INSTALL.md](INSTALL.md) for requirements, autostart, options and troubleshooting.
