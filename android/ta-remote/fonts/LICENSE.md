Bundled font attributions
=========================

FreeMono.ttf
------------
Source: GNU FreeFont project (https://www.gnu.org/software/freefont/)
License: GNU GPL v3 with font embedding exception (free for any use,
         including embedding in proprietary documents/apps).
Why bundled: Android's default `monospace` alias has neither
Latin Extended, Cyrillic / Greek, nor the Miscellaneous Technical
block (U+2300–U+23FF). FreeMono covers all of these, including
glyphs like ⏵, ⏴, ⎿ used by modern TUIs (Claude Code, Crush, etc.).
DejaVu Sans Mono was tried first but lacks the U+23xx range.

NotoEmoji.ttf
-------------
Source: Google Noto Emoji project — monochrome variant
        (https://github.com/googlefonts/noto-emoji)
License: SIL Open Font License 1.1 (OFL).
Why bundled: Android's monospace font has no emoji glyphs, so emojis
in terminal output and tab previews render as tofu (□). The
monochrome variant is ~2 MB vs. ~10 MB for Noto Color Emoji and is
sufficient for terminal context where colour isn't essential.
