# Custom intro video

The overlay can play your own video as the game's startup intro. It draws over the
splash screens shortly after launch, plays your audio track, and shows a **START GAME**
button in the bottom-right corner to skip into the game.

The intro is read from two files in the game folder at runtime, so changing it never
requires rebuilding the mod:

| File | What it is |
| --- | --- |
| `intro_full.bin` | the video, as a stream of JPEG frames + a small header |
| `intro_song.ogg` | the audio track (Ogg Vorbis) |

Both live next to `heaven_overlay.dll`. If either is missing, that part is simply skipped.

## Build the files from a video

Use `pack_intro.py`. It needs Python 3.8+ and ffmpeg (on PATH, or
`pip install imageio-ffmpeg`).

```
python pack_intro.py my_video.mp4
```

This writes `intro_full.bin` and `intro_song.ogg` to the current folder. Copy both next
to `heaven_overlay.dll` and launch the game.

### Options

```
python pack_intro.py my_video.mp4 --res 1920x1080 --fps 30 --out C:\path\to\game
```

| Option | Default | Notes |
| --- | --- | --- |
| `--res` | `2560x1440` | frame resolution `WxH` (any size) |
| `--fps` | `60` | frames per second |
| `--quality` | `4` | JPEG quality, `2` = best … `31` = worst |
| `--out` | current dir | output folder |

Any resolution works. Common choices:

```
--res 1280x720     # 720p   — smallest file, fastest to decode
--res 1920x1080    # 1080p  — good balance
--res 2560x1440    # 1440p  — sharpest (default)
```

The video is scaled to **fill the whole screen**, so pick an aspect ratio that matches
your display (16:9 is standard — all three presets above are 16:9) to avoid stretching.
The resolution only needs to be as high as your screen; going higher just makes a bigger
file with no visible gain.

### Size vs. quality

The video is decoded on the fly while playing, so only a small ring buffer of frames is
ever in memory — resolution and length mainly affect the size of `intro_full.bin` on disk
and the CPU cost of decoding. Higher resolution / fps = larger file and more decode work.
As a reference, a ~110 s clip at 2560x1440 / 60fps is around 1.2 GB on disk.

If playback stutters on a slow machine, lower `--res` or `--fps` and regenerate — no
rebuild needed.

## Removing the intro

Delete `intro_full.bin` (and/or `intro_song.ogg`) from the game folder. The overlay falls
back to the game's own startup with nothing drawn over it.

## A note on content

Heaven ships no video or audio of its own — `pack_intro.py` builds `intro_full.bin` and
`intro_song.ogg` on your machine from whatever video you give it. You're responsible for
the content you use and its licensing: don't share those two files if they're made from
material you don't have the right to redistribute.
