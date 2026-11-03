# Runs INSIDE a test VM: makes PipeWire look like a desktop with a real (driver-clocked) output device.
# The VMs have no sound card, so without this the only sink is the fallback auto_null, and a share sink that
# appears next to it becomes the default one. Undo: rm ~/.config/pipewire/pipewire.conf.d/50-fakehw.conf; restart the pipewire services.
source ~/lib.sh
pkill -x pw-cli; pkill -x firefox; sleep 1
aplay -l 2>&1 | head -3
mkdir -p ~/.config/pipewire/pipewire.conf.d
cat > ~/.config/pipewire/pipewire.conf.d/50-fakehw.conf <<'EOC'
context.objects = [
  { factory = adapter
    args = {
      factory.name     = support.null-audio-sink
      node.name        = "alsa_output.fakehw"
      node.description = "Fake hardware speakers"
      media.class      = Audio/Sink
      audio.position   = [ FL FR ]
      priority.session = 1000
      priority.driver  = 1000
      node.pause-on-idle = false
      node.always-process = true
      node.driver = true
    }
  }
]
EOC
systemctl --user restart pipewire pipewire-pulse wireplumber
sleep 4
pactl info | grep "Default Sink"
nodes
