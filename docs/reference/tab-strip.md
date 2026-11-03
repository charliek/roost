# Tab strip sizing

Roost's tab strip is a horizontally-scrolling row of pills, one per
tab in the active project. By default each pill is bounded between
a minimum and maximum width so the strip behaves like Ghostty's:
pills shrink proportionally as you open more tabs, then start
scrolling horizontally once they hit the minimum.

## Sizing

The pill width is bounded between `TAB_PILL_MIN_WIDTH = 80` and
`TAB_PILL_MAX_WIDTH = 220` points (`crates/roost-iced/src/chrome.rs`). The
active pill in edit mode (during `Cmd-R` / `Alt-R` rename) always gets at
least 220pt so the rename field is usable.

There is no config knob for these bounds. The `tab-min-width` and
`tab-max-width` keys the Swift app used to read are ignored; a
`config.conf` that still carries them is harmless.
