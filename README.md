# wlpause

Pause a video wallpaper while it is actually covered.

`mpvpaper` on a tiling compositor burns CPU forever. On a 2560x1600 panel with
a 15fps H.264 wallpaper, completely hidden behind a maximised window:

| | CPU (one core) |
|---|---|
| no pauser | **40%** |
| `wlpause` | **10%** |
| `wlpause --freeze` | **0%** |

wlpause itself measures 0%.

## Why mpvpaper's own `-p` cannot do this

mpvpaper has `--auto-pause`/`-p` and `--auto-mode MAX`. On Hyprland and other
tilers they save nothing at all — measured 37% of a core visible versus 35%
covered. This is not a bug in mpvpaper or in the compositor. Inspecting the
binary, its auto-pause has exactly two triggers:

1. **Wayland frame callbacks.** The compositor is supposed to stop asking for
   frames when a surface need not be drawn. Hyprland keeps asking, even when
   the background layer is entirely hidden behind tiled windows.
2. **`zwlr_foreign_toplevel_handle_v1` state**, which is what `-a FULL|MAX`
   watches. It looks for windows marked `fullscreen` or `maximized`. Tiled
   windows are *neither* — a window filling your whole screen still reports
   `"hasfullscreen": false`.

mpvpaper's own manual says as much: "hidden" only counts a fullscreen window,
and it "will still draw/render even if there is a normal window blocking the
wallpaper view entirely."

The root problem is that **no Wayland protocol tells a layer-shell client that
it is occluded**. Only the compositor knows, and only its own IPC will say. So
the fix has to live outside mpvpaper — which is what this is.

## What it does differently

Most of the saving comes from asking a better question. "Is there a window on
this workspace?" is the easy question and the wrong one: a small floating
calculator would freeze a wallpaper you can still see almost all of. wlpause
measures **how much of the wallpaper surface is actually covered**, by windows
*and* by opaque layer-shell surfaces such as your bar, and pauses when that
crosses a threshold.

- **Geometric coverage, not window counts.** Rectangles are subtracted rather
  than summed, so overlapping windows are never double-counted.
- **Event-driven.** It follows Hyprland's event socket and talks to the IPC
  socket directly. No `hyprctl` subprocess per tick.
- **DPMS-aware.** A wallpaper decoding to a switched-off panel is the most
  wasteful state there is.
- **Per-output.** A single `mpvpaper '*'` instance spans every output, so it
  keeps playing while *any* output still shows wallpaper.
- **`--freeze`** stops the process outright, which is the difference between
  10% and 0% (see below).
- **`--reclaim`** additionally swaps out the frozen wallpaper's memory, so
  while hidden it costs neither CPU nor (most of) its RAM.
- **Never caches the player's state.** If something else pauses the wallpaper,
  the next reconcile notices and corrects it.

### Why `--freeze` exists

Pausing mpv stops *decoding*. It does not stop the video output from redrawing
the same frame every time the compositor hands it a frame callback — and a
compositor that never realises the surface is hidden never stops handing them
out. That residual redraw is the 10%. `--freeze` additionally `SIGSTOP`s the
wallpaper process, taking it to a true zero, and `SIGCONT`s it on the way back.

It is opt-in because a stopped process cannot answer anything, including a
compositor ping. In practice it is safe: the compositor keeps displaying the
last buffer it was given, which is exactly the frozen frame you want. wlpause
resumes the wallpaper when it exits, and whenever the set of outputs changes,
so a frozen wallpaper cannot get stuck — including the awkward case of
plugging in a monitor while frozen, when a stopped process cannot create a
surface on the new output.

## Install

```sh
cargo install --git https://github.com/PercyThomas1127/wlpause
```

Requires a Rust toolchain. No other dependencies.

## Use

Start your wallpaper with an mpv IPC socket, then run wlpause:

```sh
mpvpaper -f -o 'no-audio loop-file=inf input-ipc-server=/tmp/mpvsocket' '*' video.mp4
wlpause --freeze
```

You do **not** need to tell wlpause where the socket is — it finds the
wallpaper's layer surface, reads that process's command line, and picks the
socket out of it.

Drop `-p` and `-a MAX` from mpvpaper if you had them. They do nothing useful
here, and leaving them on means two things writing mpv's `pause` property.

Hyprland, in `hyprland.conf`:

```
exec-once = mpvpaper -f -o 'no-audio loop-file=inf input-ipc-server=/tmp/mpvsocket' '*' ~/video.mp4
exec-once = wlpause --freeze
```

### `--reclaim`

A frozen process still holds all its memory. With `--reclaim`, wlpause writes
the wallpaper cgroup's `memory.current` into its `memory.reclaim` right after
freezing it, and the kernel pushes it out to swap (zswap first, if enabled).
Measured with a 2560x1600 15fps H.264 mpvpaper: anonymous memory 96MB → 0 while
frozen; on resume mpv answered IPC in 12ms and only 62MB faulted back in. GPU
buffers are pinned and stay resident. Needs swap, cgroup v2, and the wallpaper
in a cgroup **of its own** — otherwise the reclaim would hit the compositor
too, so wlpause refuses and logs why:

```
exec-once = systemd-run --user --scope --collect mpvpaper -f -o '...' '*' ~/video.mp4
exec-once = wlpause --freeze --reclaim
```

### Options

```
-s, --mpv-socket <PATH>   mpv IPC socket [default: auto-discovered]
-t, --threshold <0..1>    covered fraction at which to pause [default: 0.90]
-n, --namespace <NAME>    wallpaper layer namespace [default: mpvpaper]
    --alpha-min <0..1>    ignore layers more transparent than this [default: 1.0]
    --freeze              also SIGSTOP the wallpaper while hidden
    --reclaim             with --freeze, also swap out its memory while frozen
    --heartbeat <SECS>    re-check even without events [default: 10]
    --debounce <MS>       settle time after an event burst [default: 250]
    --once                report one decision and exit
    --dry-run             decide and log, never touch the player
-v, --verbose             log every decision
```

`--threshold` defaults to 0.90 rather than 1.0 because gaps between tiled
windows leave a few percent of wallpaper showing, and you almost certainly
want those to count as covered. Set `1.0` to pause only when literally nothing
is visible.

`--once` is the way to see what it thinks:

```
$ wlpause --once
wlpause: hyprland (event-driven), mpv at /tmp/mpvsocket, threshold 90%
  eDP-1        100.0% covered   4 occluders  on
  -> Pause
```

## Compositor support

| | status |
|---|---|
| Hyprland | supported |
| sway | planned — `GET_TREE` exposes the same rectangles |
| niri, river | possible; backends are a small trait |

Adding one means answering two questions — where is the wallpaper and what
covers it, and tell me when that might have changed. All the judgement lives
in shared code, so a backend is mostly deserialization.

## Limitations

- **Translucent windows count as opaque.** If your terminal is at 90% opacity
  the wallpaper shows through it, and wlpause will still pause behind it.
  Hyprland's IPC does not report per-window opacity, so this cannot currently
  be detected. Usually invisible in practice, especially with blur.
- **Gaps show a frozen frame.** With two or more tiled windows, the slivers of
  wallpaper between them sit on a still image. Raise `--threshold` towards 1.0
  if that bothers you.
- **mpv only.** Any wallpaper tool exposing an mpv IPC socket works; others do
  not, yet.

## Prior art

[mpvpaper-stop](https://github.com/pvtoari/mpvpaper-stop) solves the same
problem for Hyprland by polling `hyprctl` for the window count on the active
workspace, and has pywal integration this does not. wlpause differs in
measuring actual coverage, following events instead of polling a subprocess,
handling DPMS and multiple outputs, and in `--freeze`.

## Licence

MIT.
