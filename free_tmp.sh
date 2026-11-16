#!/bin/sh
# /tmp — это tmpfs на 16 ГБ, и прототип его забил распакованными архивами.
# Убираем всё, что там оставили тесты, чтобы вернуть место.
rm -rf /tmp/proto_work /tmp/lvl /tmp/lvl-19.arx /tmp/lvl-22.arx
rm -rf /tmp/bench /tmp/bench.arx /tmp/real /tmp/real-19.arx /tmp/real-12.arx /tmp/real-9.arx
rm -rf /tmp/arxtest /tmp/arxtest2 /tmp/at /tmp/at2 /tmp/at3 /tmp/at4 /tmp/at5 /tmp/at5b
rm -rf /tmp/library_gui_check /tmp/shot.png /tmp/pid
df -h /tmp | tail -1
