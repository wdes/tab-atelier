Bundled font attributions
=========================

DejaVuSansMono.ttf
------------------
Source: DejaVu Fonts project (https://dejavu-fonts.github.io/)
License: Bitstream Vera Fonts License + DejaVu Fonts public license
         (free for any use, including embedding and redistribution).
Why bundled: Android's default `monospace` alias resolves to a font
without Latin Extended / Cyrillic / Greek glyphs needed in shell
output (accented filenames, prompts, etc.).

NotoEmoji.ttf
-------------
Source: Google Noto Emoji project — monochrome variant
        (https://github.com/googlefonts/noto-emoji)
License: SIL Open Font License 1.1 (OFL).
Why bundled: Android's monospace font has no emoji glyphs, so emojis
in terminal output and tab previews render as tofu (□). The
monochrome variant is ~2 MB vs. ~10 MB for Noto Color Emoji and is
sufficient for terminal context where colour isn't essential.
