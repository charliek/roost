# Bundled light terminal themes (roost)

Byte-identical copies of four files from the iTerm2-Color-Schemes set that
Ghostty ships, taken from the pinned theme package
(`third_party/ghostty/src/build.zig.zon`, key `iterm2_themes`):

- URL: <https://deps.files.ghostty.org/ghostty-themes-release-20261019-151001-86a776d.tgz>
- Hash: `N-V-__8AAJ3vBQAq1fLMh5J8uWLkeDLS6WAdR2x5KcrmEyz6`

Files: `Catppuccin Latte`, `GitHub Light Default`, `Gruvbox Light`,
`Rose Pine Dawn`. Upstream names are kept.

They are embedded by `theme.rs` as `BUNDLED_LIGHT_THEMES`, kept apart from
`../themes/` so the desktop picker (`Theme::bundled_names`) stays dark-only.
The mobile app offers this set; `Theme::load_bundled` resolves both lists.

License. The collection is MIT (© Mark Badolato,
<https://github.com/mbadolato/iTerm2-Color-Schemes>), and each scheme keeps its
own author's license, recorded upstream in `licenses.yml`. All four are MIT:

| File | Copyright | Source |
|---|---|---|
| `Catppuccin Latte` | © 2021 Catppuccin | <https://github.com/catppuccin/palette> |
| `GitHub Light Default` | © 2020 Primer | <https://github.com/primer/github-vscode-theme> |
| `Gruvbox Light` | © Pavel Pertsev | <https://github.com/morhetz/gruvbox> |
| `Rose Pine Dawn` | © mvllow | <https://github.com/rose-pine/build> |

Check `licenses.yml` before adding a scheme: some in the set are GPL, nonfree
or unknown (`iTerm2 Solarized Light` is GPL-2.0-or-later, so it is not here).
The tarball carries no license file of its own. The MIT text:

```
MIT License

Copyright (c) <year> <copyright holders>

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

**Removal condition.** Merge these files into `../themes/` (and delete this
directory and `BUNDLED_LIGHT_THEMES`) when the desktop ships a light chrome.
