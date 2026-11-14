#!/usr/bin/env bash
# Builds a realistic test stream with ffmpeg: two qualities, a separate
# audio track, AES-128 encrypted segments and a master playlist.
# Usage: scripts/make_test_stream.sh [out_dir]
set -euo pipefail
OUT="${1:-test-stream}"
mkdir -p "$OUT/v360" "$OUT/v720" "$OUT/audio"
cd "$OUT"

head -c 16 /dev/urandom > enc.key
KEYINFO="$(cd .. && pwd)/keyinfo.tmp"
# key URI as written in the playlist, path to key file
printf '../enc.key\n%s/enc.key\n' "$(pwd)" > "$KEYINFO"

for spec in "v360 640x360 600k" "v720 1280x720 2000k"; do
  set -- $spec
  ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=$2:rate=25" -t 20 \
    -c:v libx264 -preset ultrafast -b:v "$3" -g 50 -an \
    -hls_time 4 -hls_playlist_type vod -hls_key_info_file "$KEYINFO" \
    -hls_segment_filename "$1/seg%03d.ts" "$1/index.m3u8"
done

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" -t 20 \
  -c:a aac -b:a 96k -vn \
  -hls_time 4 -hls_playlist_type vod -hls_key_info_file "$KEYINFO" \
  -hls_segment_filename "audio/seg%03d.ts" audio/index.m3u8

cat > master.m3u8 <<'EOF'
#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="audio/index.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=700000,RESOLUTION=640x360,CODECS="avc1.42c01e,mp4a.40.2",AUDIO="aud"
v360/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2100000,RESOLUTION=1280x720,CODECS="avc1.42c01f,mp4a.40.2",AUDIO="aud"
v720/index.m3u8
EOF
rm -f "$KEYINFO"
echo "test stream ready in $(pwd)"
