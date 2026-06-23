#!/usr/bin/env python3
"""Build a custom in-game intro for the Heaven MOD overlay from any video.

Produces two drop-in files that the overlay reads at runtime (no rebuild needed):

    intro_full.bin   the video, as a stream of JPEG frames + a small header
    intro_song.ogg   the audio track

Copy both next to ``heaven_overlay.dll`` in the game folder. See docs/custom-intro.md.

Requires Python 3.8+ and ffmpeg, found either on PATH or via the ``imageio-ffmpeg``
pip package (``pip install imageio-ffmpeg``).
"""

import argparse
import glob
import os
import shutil
import struct
import subprocess
import sys
import tempfile

# File header magic 'HVID' (read little-endian by the overlay) + format version.
MAGIC = 0x48564944
VERSION = 1


def find_ffmpeg():
    exe = shutil.which("ffmpeg")
    if exe:
        return exe
    try:
        import imageio_ffmpeg

        return imageio_ffmpeg.get_ffmpeg_exe()
    except Exception:
        sys.exit("ffmpeg not found. Install it on PATH, or run: pip install imageio-ffmpeg")


def main():
    ap = argparse.ArgumentParser(description="Build a custom Heaven MOD intro (video + audio).")
    ap.add_argument("video", help="source video file")
    ap.add_argument("--out", default=".", help="output folder (default: current directory)")
    ap.add_argument("--res", default="2560x1440", help="frame resolution WxH (default: 2560x1440)")
    ap.add_argument("--fps", type=int, default=60, help="frames per second (default: 60)")
    ap.add_argument(
        "--quality", type=int, default=4, help="JPEG quality, 2 = best .. 31 = worst (default: 4)"
    )
    args = ap.parse_args()

    if not os.path.isfile(args.video):
        sys.exit(f"input not found: {args.video}")
    try:
        width, height = (int(x) for x in args.res.lower().split("x"))
    except Exception:
        sys.exit("--res must look like 2560x1440")

    ff = find_ffmpeg()
    os.makedirs(args.out, exist_ok=True)
    bin_path = os.path.join(args.out, "intro_full.bin")
    ogg_path = os.path.join(args.out, "intro_song.ogg")

    tmp = tempfile.mkdtemp(prefix="intro_")
    try:
        print(f"Extracting frames @ {width}x{height} {args.fps}fps ...")
        subprocess.run(
            [ff, "-y", "-i", args.video,
             "-vf", f"fps={args.fps},scale={width}:{height}:flags=lanczos",
             "-q:v", str(args.quality),
             os.path.join(tmp, "f_%06d.jpg")],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        frames = sorted(glob.glob(os.path.join(tmp, "f_*.jpg")))
        if not frames:
            sys.exit("ffmpeg produced no frames — is the input a valid video?")

        with open(bin_path, "wb") as out:
            out.write(struct.pack("<IIIIII", MAGIC, VERSION, width, height, args.fps, len(frames)))
            for frame in frames:
                with open(frame, "rb") as fh:
                    data = fh.read()
                out.write(struct.pack("<I", len(data)))
                out.write(data)
        mb = os.path.getsize(bin_path) / 1024 / 1024
        secs = len(frames) / args.fps
        print(f"  {bin_path}  ({len(frames)} frames, {secs:.1f}s, {mb:.0f} MB)")

        print("Extracting audio ...")
        subprocess.run(
            [ff, "-y", "-i", args.video, "-vn", "-c:a", "libvorbis", "-q:a", "5", ogg_path],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        print(f"  {ogg_path}")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print("\nDone. Copy both files next to heaven_overlay.dll in the game folder.")


if __name__ == "__main__":
    main()
