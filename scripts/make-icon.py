"""Generate the original LumaDesk icon (requires Pillow for contributors only).

The checked-in ICO is compiled into the executable; end users do not need
Python or Pillow. Run this script from any directory to rebuild the artwork.
"""

from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter


SIZE = 1024
OUTPUT = Path(__file__).resolve().parent.parent / "assets"


def rect(draw, xy, radius, fill, outline=None, width=1):
    draw.rounded_rectangle(xy, radius=radius, fill=fill, outline=outline, width=width)


def create_icon():
    image = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    background = Image.new("RGBA", (SIZE, SIZE))
    pixels = background.load()
    for y in range(SIZE):
        for x in range(SIZE):
            u = x / (SIZE - 1)
            v = y / (SIZE - 1)
            pixels[x, y] = (
                int(34 - 18 * u - 12 * v),
                int(38 - 20 * u - 12 * v),
                int(77 - 26 * u - 26 * v),
                255,
            )
    mask = Image.new("L", (SIZE, SIZE))
    md = ImageDraw.Draw(mask)
    md.rounded_rectangle((24, 24, 1000, 1000), radius=220, fill=255)
    image.paste(background, (0, 0), mask)

    glow = Image.new("RGBA", image.size)
    gd = ImageDraw.Draw(glow)
    gd.ellipse((84, 10, 830, 756), fill=(112, 95, 255, 92))
    gd.ellipse((310, 370, 980, 1050), fill=(47, 221, 221, 69))
    glow = glow.filter(ImageFilter.GaussianBlur(165))
    image = Image.alpha_composite(image, glow)

    shadow = Image.new("RGBA", image.size)
    sd = ImageDraw.Draw(shadow)
    rect(sd, (167, 145, 807, 754), 94, (0, 0, 0, 106))
    shadow = shadow.filter(ImageFilter.GaussianBlur(72))
    image = Image.alpha_composite(image, shadow)

    d = ImageDraw.Draw(image)
    # Two translucent, offset fence panes suggest the organized desktop.
    rect(d, (283, 152, 827, 639), 95, (91, 79, 174, 255), (167, 148, 255, 255), 12)
    rect(d, (314, 190, 793, 284), 38, (151, 128, 245, 255))
    rect(d, (350, 222, 396, 253), 14, (229, 224, 255, 255))
    rect(d, (418, 224, 684, 247), 11, (218, 211, 255, 242))
    for y in (337, 416, 495):
        rect(d, (339, y, 749, y + 50), 22, (117, 105, 199, 255))

    rect(d, (164, 333, 721, 838), 91, (22, 44, 72, 255), (99, 234, 222, 255), 13)
    rect(d, (202, 371, 682, 463), 36, (68, 183, 186, 255))
    rect(d, (232, 398, 277, 433), 13, (229, 255, 253, 255))
    rect(d, (301, 402, 571, 429), 13, (231, 255, 255, 255))
    for y in (506, 606, 706):
        rect(d, (217, y, 662, y + 72), 27, (45, 72, 108, 255))
        rect(d, (244, y + 18, 281, y + 55), 12, (95, 237, 221, 255))
        rect(d, (309, y + 20, 578, y + 35), 7, (219, 243, 253, 241))
        rect(d, (309, y + 42, 484, y + 51), 4, (130, 180, 202, 255))

    # Re-mask for a clean translucent edge at every DPI.
    image.putalpha(ImageChops.darker(image.getchannel("A"), mask))
    return image


if __name__ == "__main__":
    OUTPUT.mkdir(parents=True, exist_ok=True)
    icon = create_icon()
    icon.resize((512, 512), Image.Resampling.LANCZOS).save(OUTPUT / "lumadesk.png")
    icon.save(OUTPUT / "lumadesk.ico", format="ICO", sizes=[
        (16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)
    ])
    print(OUTPUT / "lumadesk.ico")
