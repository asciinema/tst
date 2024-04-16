# tst - terminal streamer

This is (experimental) terminal live streaming utility for asciinema.

NOTE: this was a proof of concept, and was integrated into main [asciinema
CLI](https://github.com/asciinema/asciinema) as `asciinema stream -r` command.

## Stream via embedded HTTP server

```bash
mkfifo live.pipe

# in shell 1
tst --listen-addr 0.0.0.0:8765 -i live.pipe 

# in shell 2
asciinema rec live.pipe
```

## Stream via remote asciinema-server

```bash
mkfifo live.pipe

# in shell 1
tst --forward-url wss://asciinema.org/ws/S/<stream-producer-token> -i live.pipe 

# in shell 2
asciinema rec live.pipe
```
