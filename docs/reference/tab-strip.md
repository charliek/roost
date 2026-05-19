# Tab strip sizing

Roost's tab strip is a horizontally-scrolling row of pills, one per
tab in the active project. By default each pill is bounded between
a minimum and maximum width so the strip behaves like Ghostty's:
pills shrink proportionally as you open more tabs, then start
scrolling horizontally once they hit the minimum.

## Config keys

Set these in `~/.config/roost/config.conf` (more precisely
`$XDG_CONFIG_HOME/roost/config.conf` on both platforms). They take
effect on next launch.

| Key                | Default | Notes |
|--------------------|---------|-------|
| `tab-min-width`    | `80`    | Pill width floor in points. As more tabs open, pills shrink toward this minimum. Set to `0` to disable the floor — pills can shrink to whatever their label needs. |
| `tab-max-width`    | `220`   | Pill width cap in points. The active pill in edit mode (during `Cmd-R` rename) is unaffected — it always gets at least 220pt so the rename field is usable. Set to `0` to disable the cap entirely (pills grow to fit their full title; this is the pre-round-4 behavior). |

## Examples

```conf
# Default: pills shrink between 80pt and 220pt, then scroll.
# (No config lines needed — these are the built-in defaults.)

# Wider pills before they start shrinking:
tab-max-width = 280

# Don't shrink at all; cap pills at 200pt and scroll past that.
tab-min-width = 200
tab-max-width = 200

# Disable the cap entirely — pills grow to their full title.
# Useful if you set short, deliberate titles via `roost-cli set-title`.
tab-max-width = 0
```

## Platform notes

- **macOS** uses these config keys directly to constrain the
  `TabPillView` `widthAnchor` on each pill.
- **Linux** ignores these keys — libadwaita's `Adw.TabBar` widget
  handles tab width distribution and scrolling internally with its
  own (similar) bounded-width algorithm. The Linux behavior already
  matches the Mac default, so the keys are macOS-only for now.
