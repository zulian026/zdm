#!/usr/bin/env python3
"""Process the chosen ZDM logo (option 5, bolt arrow) into app icons.

Source: 1600x1600 RGB webp, black rounded square + lime bolt on a WHITE
background (corners are opaque white). Steps:
  1. Recover artwork coverage from the blue channel: both artwork colors
     (black B~6, lime B~66) are dark in blue, white background is B=255,
     so coverage c = (255 - B) / 189 gives correct antialiased alpha at
     the rounded-square edge and opaque artwork inside.
  2. Un-premultiply against white, then normalize artwork colors onto
     the black -> lime axis: pure black -> #000000, pure lime -> #c6f24e,
     antialiased black<->lime edges blend smoothly between them.
  3. Downscale to a 1024 master, emit all Tauri sizes + .ico, the
     sidebar asset, and the Python app icon.
"""
from PIL import Image
import numpy as np

SRC = "icon-source/media-generation-zdm-logo-opt5-bolt-arrow-0-6e759f0a-84fa-40d7-8901-86829e4e34e2.webp"
K_SRC = np.array([6.0, 6.0, 6.0])        # source black
L_SRC = np.array([200.0, 236.5, 66.0])   # source lime (avg of the two variants)
K = np.array([0.0, 0.0, 0.0])            # #000000
L = np.array([198.0, 242.0, 78.0])       # #c6f24e

im = Image.open(SRC).convert("RGB")
arr = np.asarray(im).astype(np.float64)

# 1. coverage from blue channel
c = np.clip((255.0 - arr[..., 2]) / 189.0, 0.0, 1.0)
# 2. un-premultiply against white, then project onto black->lime axis
safe_c = np.maximum(c, 0.05)
art = (arr - (1.0 - c)[..., None] * 255.0) / safe_c[..., None]
art = np.clip(art, 0.0, 255.0)
v = art - K_SRC
axis = L_SRC - K_SRC
t = np.clip((v @ axis) / (axis @ axis), 0.0, 1.0)[..., None]
rgb = K + t * (L - K)
rgba = np.dstack([rgb, (c * 255.0)]).astype(np.uint8)
img = Image.fromarray(rgba, "RGBA")

master = img.resize((1024, 1024), Image.LANCZOS)
master.save("icon-source/zdm-icon-1024.png")

outputs = [
    ("src-tauri/icons/icon.png", 1024),
    ("src-tauri/icons/128x128@2x.png", 256),
    ("src-tauri/icons/128x128.png", 128),
    ("src-tauri/icons/32x32.png", 32),
]
for path, size in outputs:
    master.resize((size, size), Image.LANCZOS).save(path)

master.save(
    "src-tauri/icons/icon.ico",
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)
master.save("src/assets/zdm-icon.png")
master.save("/home/hatch/workspace/download-manager-linux/icon.png")

# 32px readability preview, enlarged with NEAREST so pixels stay visible
prev32 = master.resize((32, 32), Image.LANCZOS)
prev32.resize((256, 256), Image.NEAREST).save("/tmp/zdm-bolt-32-preview.png")
# master on white background (shows the transparent corners as white)
white_bg = Image.new("RGBA", (1024, 1024), (255, 255, 255, 255))
white_bg.alpha_composite(master)
white_bg.convert("RGB").resize((512, 512), Image.LANCZOS).save("/tmp/zdm-bolt-master-preview.png")

# sanity report
a = np.asarray(master)
print("master:", master.size, master.mode)
print("corners alpha:", [int(a[y, x, 3]) for y, x in [(0, 0), (1023, 0), (0, 1023), (1023, 1023)]])
print("edge-center alpha:", [int(a[y, x, 3]) for y, x in [(0, 512), (512, 0), (1023, 512), (512, 1023)]])
opaque = a[..., 3] > 250
cols = np.unique(a[opaque][..., :3].reshape(-1, 3), axis=0)
print("opaque unique colors (sample):", cols[:8].tolist(), "... total", len(cols))
print("32px non-transparent px:", int((np.asarray(prev32)[..., 3] > 128).sum()), "of 1024")
