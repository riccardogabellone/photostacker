# Runs inside GIMP 3 (batch interpreter python-fu-eval), with no window; standins.mjs
# starts it. Opens each file of PS_GIMP_FILES ('|'-separated) and appends one JSON line
# per file to PS_GIMP_OUT: what GIMP made of it (layers, names, flags, profile) and a
# digest of every layer's stored samples, channel by channel.
import hashlib
import json
import os
import time
import traceback

import gi

gi.require_version("Gimp", "3.0")
gi.require_version("Gegl", "0.4")
from gi.repository import Gegl, Gimp, Gio  # noqa: E402

OUT = os.environ["PS_GIMP_OUT"]
FILES = [p for p in os.environ["PS_GIMP_FILES"].split("|") if p]
# A layer with more raw bytes than this is listed but not hashed.
PIXEL_LIMIT = int(os.environ.get("PS_GIMP_PIXEL_LIMIT", str(700 * 1024 * 1024)))


def nick(value):
    try:
        return value.value_nick
    except Exception:
        return str(value)


def guarded(record, key, call):
    try:
        record[key] = call()
    except Exception as error:  # the API differs a little between GIMP 3 releases
        record[key + "_error"] = repr(error)


def read(drawable, precision):
    """The drawable's stored samples: (bytes, bytes per pixel, bytes per sample)."""
    width, height = drawable.get_width(), drawable.get_height()
    sample_bytes = 1 if precision.startswith("u8") else 2 if precision.startswith("u16") else 4
    if width * height * sample_bytes * 4 > PIXEL_LIMIT:
        return None
    rect = Gegl.Rectangle.new(0, 0, width, height)
    # No format name: the buffer's own format, so no conversion touches the samples.
    data = drawable.get_buffer().get(rect, 1.0, None, Gegl.AbyssPolicy.NONE)
    return data, len(data) // (width * height), sample_bytes


def describe_pixels(drawable, raw):
    """One SHA-1 per channel of the stored samples, and a few of the samples."""
    if raw is None:
        return {"skipped": "too large to hash"}
    data, bytes_per_pixel, sample_bytes = raw
    width = drawable.get_width()
    height = drawable.get_height()
    channels = bytes_per_pixel // sample_bytes
    planes = []
    for channel in range(channels):
        digest = hashlib.sha1()
        for byte in range(sample_bytes):
            digest.update(data[channel * sample_bytes + byte :: bytes_per_pixel])
        planes.append(digest.hexdigest()[:16])
    out = {"channels": channels, "sample_bytes": sample_bytes, "planes": planes}

    def value_at(x, y):
        start = (y * width + x) * bytes_per_pixel
        return [
            int.from_bytes(data[start + c * sample_bytes : start + (c + 1) * sample_bytes], "little")
            for c in range(channels)
        ]

    out["samples"] = {
        "top_left": value_at(0, 0),
        "centre": value_at(width // 2, height // 2),
        "quarter": value_at(width // 4, height // 4),
        "bottom_right": value_at(width - 1, height - 1),
    }
    if channels in (2, 4):  # the last channel is alpha; its high byte is enough here
        alpha = data[(channels - 1) * sample_bytes + sample_bytes - 1 :: bytes_per_pixel]
        out["alpha_high_byte"] = [min(alpha), max(alpha)]
    return out


def colour_difference(top, other):
    """The largest colour difference between two RGBA drawables' samples, over the
    pixels where the first is at least half opaque. A reader that undoes the blend
    against white of a PSD's merged image gets the top layer back only up to rounding,
    and the rounding grows as alpha falls, so the nearly transparent pixels are left out."""
    if top is None or other is None:
        return None
    (a_data, a_bpp, a_bytes), (b_data, b_bpp, b_bytes) = top, other
    if (a_bpp, a_bytes) != (b_bpp, b_bytes) or a_bpp != 4 * a_bytes or len(a_data) != len(b_data):
        return None
    code = "B" if a_bytes == 1 else "H"
    a, b = memoryview(a_data).cast(code), memoryview(b_data).cast(code)
    half = (255 if a_bytes == 1 else 65535) // 2
    worst = counted = 0
    for i in range(0, len(a), 4):
        if a[i + 3] >= half:
            counted += 1
            for c in range(3):
                d = a[i + c] - b[i + c]
                if d < 0:
                    d = -d
                if d > worst:
                    worst = d
    return {"worst": worst, "pixels": counted}


def describe_layer(layer, precision, keep=None):
    record = {}
    guarded(record, "name", layer.get_name)
    guarded(record, "size", lambda: [layer.get_width(), layer.get_height()])
    guarded(record, "offsets", lambda: list(layer.get_offsets())[-2:])
    guarded(record, "visible", layer.get_visible)
    guarded(record, "opacity", layer.get_opacity)
    guarded(record, "mode", lambda: nick(layer.get_mode()))
    guarded(record, "has_alpha", layer.has_alpha)
    guarded(record, "is_group", layer.is_group)
    guarded(record, "mask", lambda: layer.get_mask() is not None)
    guarded(
        record,
        "locks",
        lambda: {
            "content": layer.get_lock_content(),
            "position": layer.get_lock_position(),
            "alpha": layer.get_lock_alpha(),
            "visibility": layer.get_lock_visibility(),
        },
    )

    def pixels():
        raw = read(layer, precision)
        if keep is not None:
            keep.append(raw)
        return describe_pixels(layer, raw)

    guarded(record, "pixels", pixels)
    return record


def describe_image(image, keep_top=None):
    record = {}
    guarded(record, "size", lambda: [image.get_width(), image.get_height()])
    guarded(record, "base_type", lambda: nick(image.get_base_type()))
    guarded(record, "precision", lambda: nick(image.get_precision()))
    guarded(record, "resolution", lambda: list(image.get_resolution())[-2:])

    def profile():
        found = image.get_color_profile()
        if found is None:
            return None
        return {"label": found.get_label(), "linear": found.is_linear()}

    guarded(record, "profile", profile)
    precision = record.get("precision", "u8")
    layers = image.get_layers()
    record["layers"] = [
        describe_layer(layer, precision, keep_top if index == 0 else None) for index, layer in enumerate(layers)
    ]

    def merged():
        copy = image.duplicate()
        try:
            flat = copy.merge_visible_layers(Gimp.MergeType.CLIP_TO_IMAGE)
            return describe_pixels(flat, read(flat, precision))
        finally:
            copy.delete()

    if len(layers) > 1:
        guarded(record, "merged_visible", merged)
    return record


def load(path, procedure_name=None):
    file = Gio.File.new_for_path(path)
    if procedure_name is None:
        return Gimp.file_load(Gimp.RunMode.NONINTERACTIVE, file)
    procedure = Gimp.get_pdb().lookup_procedure(procedure_name)
    config = procedure.create_config()
    config.set_property("run-mode", Gimp.RunMode.NONINTERACTIVE)
    config.set_property("file", file)
    result = procedure.run(config)
    if result.index(0) != Gimp.PDBStatusType.SUCCESS:
        raise RuntimeError("%s: %s" % (procedure_name, Gimp.get_pdb().get_last_error()))
    return result.index(1)


def check(path):
    record = {"file": os.path.basename(path), "gimp": Gimp.version()}
    started = time.time()
    top = []
    try:
        image = load(path)
        if image is None:
            record["error"] = "not opened: %s" % Gimp.get_pdb().get_last_error()
        else:
            record.update(describe_image(image, top))
            image.delete()
    except Exception:
        record["error"] = traceback.format_exc()
    if path.lower().endswith((".psd", ".psb")) and "error" not in record:
        # The merged image the file stores, as GIMP reads it when asked for no layers.
        try:
            image = load(path, "file-psd-load-merged")
            stored = []
            record["stored_composite"] = describe_image(image, stored)
            if top and stored:
                guarded(record, "stored_composite_vs_top", lambda: colour_difference(top[0], stored[0]))
            image.delete()
        except Exception:
            record["stored_composite_error"] = traceback.format_exc()
    record["seconds"] = round(time.time() - started, 1)
    with open(OUT, "a", encoding="utf-8") as out:
        out.write(json.dumps(record, ensure_ascii=False) + "\n")


for each in FILES:
    check(each)
