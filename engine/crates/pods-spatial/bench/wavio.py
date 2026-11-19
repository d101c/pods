"""Tiny float32 WAV reader/writer (no soundfile on this machine)."""
import struct
import numpy as np


def write_wav(path, data, rate=48000):
    """data: (n, ch) float array, written as 32-bit IEEE float."""
    data = np.asarray(data, dtype="<f4")
    if data.ndim == 1:
        data = data[:, None]
    n, ch = data.shape
    raw = data.tobytes()
    fmt = struct.pack("<HHIIHH", 3, ch, rate, rate * ch * 4, ch * 4, 32)
    with open(path, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", 4 + 8 + len(fmt) + 8 + len(raw)) + b"WAVE")
        f.write(b"fmt " + struct.pack("<I", len(fmt)) + fmt)
        f.write(b"data" + struct.pack("<I", len(raw)) + raw)


def read_wav(path):
    """Returns (rate, (n, ch) float64 array). Handles PCM16/24/32 and float32."""
    with open(path, "rb") as f:
        buf = f.read()
    assert buf[:4] == b"RIFF" and buf[8:12] == b"WAVE", path
    pos, fmt, data = 12, None, None
    while pos + 8 <= len(buf):
        cid, size = buf[pos:pos + 4], struct.unpack("<I", buf[pos + 4:pos + 8])[0]
        body = buf[pos + 8:pos + 8 + size]
        if cid == b"fmt ":
            fmt = struct.unpack("<HHIIHH", body[:16])
            if fmt[0] == 0xFFFE:  # WAVE_FORMAT_EXTENSIBLE: real tag in the GUID
                fmt = (struct.unpack("<H", body[24:26])[0],) + fmt[1:]
        elif cid == b"data":
            data = body if size != 0xFFFFFFFF and size else buf[pos + 8:]
            break
        pos += 8 + size + (size & 1)
    tag, ch, rate, _, _, bits = fmt
    if tag == 3 and bits == 32:
        x = np.frombuffer(data[: len(data) // (4 * ch) * 4 * ch], "<f4").astype(np.float64)
    elif tag == 1 and bits == 16:
        x = np.frombuffer(data[: len(data) // (2 * ch) * 2 * ch], "<i2") / 32768.0
    elif tag == 1 and bits == 32:
        x = np.frombuffer(data[: len(data) // (4 * ch) * 4 * ch], "<i4") / 2147483648.0
    elif tag == 1 and bits == 24:
        b = np.frombuffer(data[: len(data) // (3 * ch) * 3 * ch], np.uint8).reshape(-1, 3)
        v = (b[:, 0].astype(np.int32) | (b[:, 1].astype(np.int32) << 8) | (b[:, 2].astype(np.int32) << 16))
        v = np.where(v >= 1 << 23, v - (1 << 24), v)
        x = v / float(1 << 23)
    else:
        raise ValueError("unsupported wav %r" % (fmt,))
    return rate, x.reshape(-1, ch)
