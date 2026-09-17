# CFF2 regression fixture

`cff2-test.otf` is an original synthetic font for tests, under the repository's
license. Its visible glyph is a rectangle from (100, 0) to (500, 700), with a
600-unit advance on a 1000-unit em. ASCII A and the CJK probe characters
あ, ア and 漢 all map to that rectangle; space has no outline.

It uses CFF2 charstrings to exercise the same outline parser as Android's
variable Noto CJK fonts, without including a system font in the repository.
