from PIL import Image, ImageDraw
im = Image.new("RGB", (1000, 700), (70, 90, 60))
d = ImageDraw.Draw(im)
d.rectangle([400, 0, 420, 500], fill=(120, 120, 120))
for x in range(0, 1000, 50):
    d.line([x, 0, x, 700], fill=(80, 100, 70))
im.save("/tmp/e2e/overlay.png")
