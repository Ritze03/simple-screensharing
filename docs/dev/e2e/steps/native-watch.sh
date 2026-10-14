click 387 380
sleep 6
move 495 134
sleep 2.5
shot tip
echo "--- sink inputs"
pactl list sink-inputs 2>/dev/null | grep -E "Sink Input|application.name|application.process.binary|media.name|Corked|Mute" | head -40
echo "--- peak monitor (3s of the app's own sink input, via pw-dump node count)"
pw-dump 2>/dev/null | python3 -c '
import sys,json
d=json.load(sys.stdin)
for o in d:
    p=o.get("info",{}).get("props",{})
    if o.get("type")=="PipeWire:Interface:Node" and "screensharing" in json.dumps(p).lower():
        print(o["id"], p.get("node.name"), p.get("media.class"), o["info"].get("state"))
'
sleep 2
shot end
