"""Turns a recording made by `cargo run --example record` into a GIF, drawn the way Windows
Terminal draws it: Campbell colours, Cascadia Mono, bold as the bright colour, a bar cursor.

    python -I tools/demo/render.py RECORDING.json OUT.gif [--speed 1.0] [--png FIRST.png]
"""
import argparse
import json
import os
import re
import sys

from PIL import Image, ImageDraw, ImageFont

CAMPBELL = [
    "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00", "#0037da", "#881798", "#3a96dd", "#cccccc",
    "#767676", "#e74856", "#16c60c", "#f9f1a5", "#3b78ff", "#b4009e", "#61d6d6", "#f2f2f2",
]
DEFAULT_FG, DEFAULT_BG, CURSOR = "#cccccc", "#0c0c0c", "#ffffff"
BOLD, DIM, ITALIC, UNDERLINE, INVERSE, WIDE = 1, 2, 4, 8, 16, 32


def rgb(text):
    return tuple(int(text[i:i + 2], 16) for i in (1, 3, 5))


def xterm(index):
    if index < 16:
        return rgb(CAMPBELL[index])
    if index < 232:
        index -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return steps[index // 36], steps[index // 6 % 6], steps[index % 6]
    grey = 8 + (index - 232) * 10
    return grey, grey, grey


def colour(spec, default, bright=False):
    if not spec:
        return rgb(default)
    if spec.startswith("#"):
        return rgb(spec)
    index = int(spec[1:])
    if bright and index < 8:
        index += 8
    return xterm(index)


def find_font():
    for path in (
        os.environ.get("DEMO_FONT", ""),
        r"C:\Windows\Fonts\CascadiaMono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    ):
        if path and os.path.exists(path):
            return path
    sys.exit("no monospace font found: set DEMO_FONT")


class Painter:
    def __init__(self, cell_width):
        path = find_font()
        probe = ImageFont.truetype(path, 100)
        # A size at which a character is exactly cell_width pixels wide, so whole runs can be
        # drawn at once and still sit on the grid.
        size = 100 * cell_width / probe.getlength("M")
        self.font = ImageFont.truetype(path, size)
        ascent, descent = self.font.getmetrics()
        self.cw, self.ch = cell_width, ascent + descent
        self.fallback = None
        for path in (r"C:\Windows\Fonts\seguisym.ttf",):
            if os.path.exists(path):
                self.fallback = ImageFont.truetype(path, size * 0.9)
        self.missing = {}

    def glyph(self, char):
        image = Image.new("L", (self.cw * 2, self.ch))
        ImageDraw.Draw(image).text((0, 0), char, font=self.font, fill=255)
        return image.tobytes()

    def has_glyph(self, char):
        if char not in self.missing:
            # A character the font lacks comes out as the same box as a private-use one.
            self.missing[char] = char != " " and self.glyph(char) == self.glyph("\ue0ff")
        return not self.missing[char]

    def frame(self, data, frame):
        image = Image.new("RGB", (data["cols"] * self.cw, data["rows"] * self.ch), rgb(DEFAULT_BG))
        draw = ImageDraw.Draw(image)
        for row, runs in enumerate(frame["rows"]):
            col = 0
            y = row * self.ch
            for text, fg_spec, bg_spec, bits in runs:
                width = 2 if bits & WIDE else len(text)
                fg = colour(fg_spec, DEFAULT_FG, bright=bool(bits & BOLD))
                bg = colour(bg_spec, DEFAULT_BG)
                if bits & INVERSE:
                    fg, bg = bg, fg
                if bits & DIM:
                    fg = tuple((f + b) // 2 for f, b in zip(fg, bg))
                x = col * self.cw
                draw.rectangle([x, y, x + width * self.cw - 1, y + self.ch - 1], fill=bg)
                if text.strip():
                    if all(self.has_glyph(c) for c in text):
                        draw.text((x, y), text, font=self.font, fill=fg)
                    else:
                        for i, c in enumerate(text):
                            font = self.font if self.has_glyph(c) or not self.fallback else self.fallback
                            draw.text((x + i * self.cw, y), c, font=font, fill=fg)
                if bits & UNDERLINE:
                    draw.line([x, y + self.ch - 2, x + width * self.cw - 1, y + self.ch - 2], fill=fg)
                col += width
        if frame["cursor"]:
            r, c = frame["cursor"]
            draw.rectangle([c * self.cw, r * self.ch, c * self.cw + 1, (r + 1) * self.ch - 1], fill=rgb(CURSOR))
        return image


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("recording")
    parser.add_argument("gif")
    parser.add_argument("--cell", type=int, default=10, help="cell width in pixels")
    parser.add_argument("--speed", type=float, default=1.0)
    parser.add_argument("--png", help="also write the last frame as a PNG")
    parser.add_argument("--forbid", action="append", default=[],
                        help="a regular expression no screen may show (a real path, a name); repeatable")
    args = parser.parse_args()
    data = json.load(open(args.recording, encoding="utf-8"))
    for i, frame in enumerate(data["frames"]):
        text = "\n".join("".join(run[0] for run in row) for row in frame["rows"])
        for pattern in args.forbid:
            found = re.search(pattern, text)
            if found:
                sys.exit(f"{args.recording}: screen {i} (at {frame['t']} ms) shows {found.group(0)!r}: no GIF written")
    painter = Painter(args.cell)
    frames = data["frames"]
    images, durations = [], []
    for i, frame in enumerate(frames[:-1]):
        shown = (frames[i + 1]["t"] - frame["t"]) / args.speed
        if shown < 20 and images:
            # Too short to see: the next screen replaces it.
            continue
        images.append(painter.frame(data, frame).quantize(colors=256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE))
        durations.append(max(20, round(shown / 10) * 10))
    if not images:
        images.append(painter.frame(data, frames[-1]).quantize(colors=256, dither=Image.Dither.NONE))
        durations.append(1000)
    images[0].save(args.gif, save_all=True, append_images=images[1:], duration=durations, loop=0, optimize=True, disposal=1)
    if args.png:
        painter.frame(data, frames[-1]).save(args.png, optimize=True)
    size = os.path.getsize(args.gif)
    print(f"{args.gif}: {len(images)} frames, {sum(durations) / 1000:.1f} s, {size // 1024} KB, {images[0].size[0]}x{images[0].size[1]}")


if __name__ == "__main__":
    main()
