# Local codec fixtures

One second of generated video (128×96, 10 frames/second, H.264 Constrained
Baseline level 3.0) and silent AAC-LC stereo audio. These files contain no
third-party media. Fragmented MP4 permits the same bytes to exercise both
ordinary HTML media playback and MediaSource/SourceBuffer decoding.

Regenerate with FFmpeg (only needed to change the fixtures):

```sh
ffmpeg -f lavfi -i testsrc2=size=128x96:rate=10 -t 1 -an \
  -c:v libx264 -profile:v baseline -level:v 3.0 -pix_fmt yuv420p -g 10 \
  -movflags +frag_keyframe+empty_moov+default_base_moof h264.mp4
ffmpeg -f lavfi -i anullsrc=r=48000:cl=stereo -t 1 -vn -c:a aac -b:a 64k \
  -movflags +frag_keyframe+empty_moov+default_base_moof aac.mp4
ffmpeg -i h264.mp4 -i aac.mp4 -c copy \
  -movflags +frag_keyframe+empty_moov+default_base_moof h264-aac.mp4
```

`python3 tools/test_media_playback.py` uses the committed files directly; it
does not require FFmpeg, an internet connection, or access to a streaming site.
