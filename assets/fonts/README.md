# Fonts

`InterVariable.ttf` is [Inter](https://github.com/rsms/inter) 4.1 (font version 4.001, SIL Open Font License 1.1, see `Inter-LICENSE.txt`) with its `tnum` feature frozen into the character map. egui cannot select OpenType features, so every digit now maps to its fixed-width glyph: counters that change every frame keep their width instead of jittering. The idea comes from fastframe-fonts (https://fastframe.dev/fastframe-fonts/).

Rebuild from the upstream release:
```sh
gh release download -R rsms/inter v4.1 -p Inter-4.1.zip && unzip Inter-4.1.zip -d inter
uvx --from opentype-feature-freezer pyftfeatfreeze -f tnum inter/InterVariable.ttf InterVariable.ttf
```
The freeze changes only which glyph a codepoint maps to; names, axes (`opsz`, `wght`) and metrics stay as upstream. A test in `crates/ui/src/theme.rs` checks that all ten digits lay out at the same width.
