# Images in the terminal

Roost draws images that programs send with the
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/):
`kitten icat`, `chafa -f kitty`, `timg`, yazi's previews, and plotting
backends that print to the terminal. Roost-Iced draws them, on Linux and on
macOS. The older Swift Roost.app doesn't.

## Trying it

```sh
kitten icat --detect-support   # reports the transfer mode it will use
kitten icat photo.png
```

`kitten icat` sizes an image from the terminal's pixel size, which Roost
reports as the grid times the cell size in device pixels (see
[Sizes are in device pixels](#sizes-are-in-device-pixels)). It probes for a
file, temporary-file or shared-memory transfer first. Roost refuses those, so
it falls back to sending the image in-band. `kitten icat --transfer-mode=stream`
skips the probe.

## What works

- **Formats.** RGB, RGBA and PNG (`f=24`, `f=32`, `f=100`). PNG is decoded as
  it arrives.
- **Placements.** Placing an image at the cursor, at an offset inside a cell,
  scaled to a cell span (`c=`, `r=`), or cropped to part of the source image
  (`x=`, `y=`, `w=`, `h=`). Unicode-placeholder placements (`U=1`) work too.
  An image moves with its text as the screen scrolls and is clipped at the
  grid's edge.
- **Layers.** A negative `z` draws an image under the text. Below
  −1073741824 it also draws under cell backgrounds, so an explicit background
  colour covers it. A `z` of 0 or more draws it over the text.
- **Deleting.** `a=d` with a lowercase target removes placements and keeps the
  image for later use. An uppercase one (`d=I`, `d=A`) frees the image too.

## Limits

- **In-band transfer only.** The file (`t=f`), temporary-file (`t=t`) and
  shared-memory (`t=s`) media are off. They name a resource on the machine
  running the shell, which a Roost window attached to that tab from another
  machine can't read. Programs that probe for them fall back to in-band
  transfer, as `kitten icat` does.
- **32 MiB per screen.** That is about four 1920×1080 RGBA images. When a new
  image would go over the limit, the oldest images are evicted first.
- **Animations show one frame.** An animated image shows its current frame,
  and the frame doesn't advance on its own.
- **A large image's first frame.** On the GPU renderer, an image of 2 MiB or
  more uploads in the background, so it appears one frame after the text
  around it.

## Sizes are in device pixels

A terminal's cell size in pixels is its cell size in points times the
display's scale factor, rounded: on a Retina display at scale 2, a cell 8
points wide is 16 pixels. An image therefore shows at its native size and stays
sharp, as in kitty and Ghostty. Everything that reports the terminal's size in
pixels uses the same number:

- the PTY's window size (`TIOCGWINSZ`), which `kitten icat` reads;
- the replies to the size queries `CSI 14 t` (the text area in pixels) and
  `CSI 16 t` (the cell in pixels);
- the in-band size reports of mode 2048.

On a HiDPI display those report twice what Roost reported before images were
drawn. Mouse reports in pixel mode (mode 1016) still use points.

When the window moves to a display with another scale factor, each local tab
whose cell size in pixels changes is resized, and its program gets a
`SIGWINCH`. A full-screen program redraws, as it does after any resize. A
change small enough to round to the same pixel size changes nothing.

A tab on a session follows the window only when the window sizes it. If the
window is focused, the move takes the size as a resize would. If another
client sizes the tab and the window isn't focused, the window only tells the
session the size it would like: the tab keeps the session's pixel size and its
program isn't resized. The scale a window opens at never takes a tab either.

## Tabs on a session

A tab on a `roost-session` (Roost-Iced's local session, or a saved host) draws
images in the same way. The session keeps the terminal state, so an image on
screen is still there after you detach and attach again, or reconnect.

- **The window that sizes the tab sets its pixel size.** A session tab's cell
  size comes from the window that sizes it, in that window's device pixels.
  Another window showing the same tab draws the images scaled to its own cells,
  so on a display with a lower scale factor they look softer.
- **The `vt` fallback carries no images.** A tab attached in
  [reduced fidelity](host-sessions.md) shows images only once the program
  sends them again.
- **Until the tab is sized, its pixel size is 0.** A tab nobody has sized yet
  has no cell size in pixels, so a program asking at that moment sees 0 and
  draws nothing. The same goes for a program that asks before a new tab's first
  layout, such as an `icat` in a shell startup file.
