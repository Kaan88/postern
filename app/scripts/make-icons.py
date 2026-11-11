"""Draws Postern's icons from the mark's geometry (64-unit grid) with Pillow.

Outputs (into app/icons and app/installer):
  icons/icon-1024.png        source for `cargo tauri icon` (ico, icns, png set)
  icons/tray.png, tray-unread.png, tray-offline.png   32 px tray states
  installer/nsis-header.bmp  150x57, installer/nsis-sidebar.bmp 164x314
  installer/wix-banner.bmp   493x58, installer/wix-dialog.bmp 493x312

Run: python scripts/make-icons.py   (from app/)
"""
from pathlib import Path
from PIL import Image, ImageDraw

RED = (219, 45, 84)        # Bulwark's brand red, shared on purpose: Postern is a companion
RED_DARK = (166, 25, 58)
GREY = (125, 129, 144)
WHITE = (255, 255, 255)
INK = (41, 42, 57)
DOT = (255, 207, 63)
ROOT = Path(__file__).resolve().parent.parent
SS = 4  # supersampling


def mark(size, tile=RED, door=WHITE, unread=False):
    s = size * SS / 64
    img = Image.new("RGBA", (size * SS, size * SS), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([4 * s, 4 * s, 60 * s, 60 * s], radius=14 * s, fill=tile)
    # A crenellated wall with a small arched gate in it: the postern.
    d.rectangle([13 * s, 27 * s, 51 * s, 50 * s], fill=door)
    for x in (13, 28, 43):
        d.rectangle([x * s, 16 * s, (x + 8) * s, 28 * s], fill=door)
    d.rectangle([25 * s, 43 * s, 39 * s, 50.5 * s], fill=tile)
    d.pieslice([25 * s, 36 * s, 39 * s, 50 * s], 180, 360, fill=tile)
    if unread:
        # Cut a ring so the dot reads on light and dark taskbars alike.
        d.ellipse([39 * s, -1 * s, 65 * s, 25 * s], fill=(0, 0, 0, 0))
        d.ellipse([43 * s, 3 * s, 61 * s, 21 * s], fill=DOT)
    return img.resize((size, size), Image.LANCZOS)


def bmp(path, size, draw):
    img = Image.new("RGB", size, WHITE)
    draw(img)
    path.parent.mkdir(parents=True, exist_ok=True)
    img.save(path, "BMP")


def sidebar(img):
    w, h = img.size
    d = ImageDraw.Draw(img)
    for y in range(h):  # red, darkening towards the bottom
        t = y / h
        d.line([(0, y), (w, y)], fill=tuple(int(a + (b - a) * t) for a, b in zip(RED, RED_DARK)))
    m = mark(96, tile=WHITE, door=RED)
    img.paste(m, ((w - 96) // 2, 70), m)


def header(img):
    w, h = img.size
    m = mark(44)
    img.paste(m, (w - 44 - 8, (h - 44) // 2), m)


def banner(img):
    w, h = img.size
    ImageDraw.Draw(img).rectangle([0, h - 3, w, h], fill=RED)
    m = mark(42)
    img.paste(m, (w - 42 - 10, (h - 42) // 2 - 1), m)


def dialog(img):
    w, h = img.size
    d = ImageDraw.Draw(img)
    d.rectangle([0, 0, 164, h], fill=RED)  # WiX draws its text right of 164 px
    m = mark(96, tile=WHITE, door=RED)
    img.paste(m, ((164 - 96) // 2, 60), m)


def main():
    icons = ROOT / "icons"
    mark(1024).save(icons / "icon-1024.png")
    mark(32).save(icons / "tray.png")
    mark(32, unread=True).save(icons / "tray-unread.png")
    mark(32, tile=GREY).save(icons / "tray-offline.png")
    inst = ROOT / "installer"
    bmp(inst / "nsis-header.bmp", (150, 57), header)
    bmp(inst / "nsis-sidebar.bmp", (164, 314), sidebar)
    bmp(inst / "wix-banner.bmp", (493, 58), banner)
    bmp(inst / "wix-dialog.bmp", (493, 312), dialog)
    print("icons and installer bitmaps written")


if __name__ == "__main__":
    main()
