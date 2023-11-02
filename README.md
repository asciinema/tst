# tst - terminal streamer

This is (experimental) terminal live streaming utility for asciinema.

## Stream via embedded HTTP server

```bash
mkfifo live.pipe

# in shell 1
tst --listen-addr 0.0.0.0:8765 live.pipe 

# in shell 2
asciinema rec live.pipe
```

## Stream via remote asciinema-server

```bash
mkfifo live.pipe

# in shell 1
tst --forward-url https://asciinema.org/s/<stream-producer-token> live.pipe 

# in shell 2
asciinema rec live.pipe
```
