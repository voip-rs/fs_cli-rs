# fs_cli-rs

Interactive FreeSWITCH CLI client written in Rust using
[freeswitch-esl-tokio](https://github.com/voip-rs/freeswitch-esl-tokio).

## Features

- Readline with command history, search, and tab completion via `console_complete`
- Colorized log and command output (configurable: `never`, `tag`, `line`)
- YAML configuration profiles (`~/.config/fs_cli.yaml`, `/etc/freeswitch/fs_cli.yaml`)
- Automatic reconnection on connection loss (`-R`)
- Event subscription on startup (`--events`), gating the idle-liveness timer
- Userauth support (`-u user@domain`)
- Non-interactive mode (`-x "command"` and `-X "background command"`, repeatable and interleaved)
- The FreeSWITCH log stream on a file or on stdout (`--log-file`), in any mode

The boolean flags `-r`/`--retry`, `-R`/`--reconnect`, `--events`, and
`-q`/`--quiet` take an optional value, so a profile default can be
overridden explicitly (`-r false`) instead of only ever being turned on.

## Installation

Pre-built binaries for Linux AMD64/ARM64 and Windows are available on the
[releases page](https://github.com/voip-rs/fs_cli-rs/releases). From v1.4.3 they
are built on Debian Bullseye and need glibc 2.30 or newer; on Debian Buster and
other glibc 2.28 hosts, use
[v1.4.2](https://github.com/voip-rs/fs_cli-rs/releases/tag/v1.4.2) or build from
source.

To build from source:

```sh
cargo build --release
```

## Usage

```sh
# Default connection (localhost:8021, password ClueCon)
fs_cli

# Remote host with profile
fs_cli -H 192.168.1.100 -P 8021 -p mypassword

# Userauth
fs_cli -u admin@default -p secret

# Non-interactive
fs_cli -x "sofia status" -x "show channels"

# Use a named profile from config
fs_cli production
```

Run `fs_cli --help` for full options, `fs_cli --list-profiles` to see
configured profiles.

## Non-interactive modes

What fs_cli does with no `-x`/`-X` depends on whether it has a terminal, because interactive mode needs one on both stdin and stdout.

| commands | stdin and stdout a tty | `--log-file` | mode |
|---|---|---|---|
| yes | either | optional | run them in order, then exit |
| no | yes | optional | interactive, teeing the log stream to the file when given |
| no | no | yes | stream log lines until SIGINT or SIGTERM |
| no | no | no | error, exit 1 |

`-x` and `-X` run in the order they were typed, whichever flag they came from. `-x` sends an api command and prints its reply. `-X` sends a bgapi command, whose result arrives later as a `BACKGROUND_JOB` event; fs_cli subscribes to that event and matches it by Job-UUID, which is what makes the result visible at all. The match is also what makes it correct: `BACKGROUND_JOB` is fired on the global event bus, so every ESL client sees every other client's job results.

A job result prints as `[command] result` once it arrives. Log lines emitted while the batch runs land on the `--log-file` destination between commands.

After the last command fs_cli waits for any outstanding job, forever unless `--job-timeout MS` says otherwise. A refused command or a failed job prints and exits 0, as `-x` has always done; a transport fault or an expired `--job-timeout` exits 1, the latter naming the jobs that never reported.

`--log-file -` writes the log stream to stdout, honouring `--color`; a real file is never coloured. Both are command-line options with no profile key.

```sh
# api, bgapi and api, in that order, with the job result printed when it lands
fs_cli -x status -X 'sofia status' -x uptime

# batch with the log stream captured, giving up on jobs after 5 seconds
fs_cli --log-file /var/log/fs_batch.log --job-timeout 5000 -X 'reloadxml'

# follow the log stream until interrupted
fs_cli --log-file - -l notice
```

## Configuration

Search order: `~/.config/fs_cli.yaml`, `~/.fs_cli.yaml`,
`/etc/freeswitch/fs_cli.yaml`, then the C fs_cli files `~/.fs_cli_conf` and
`/etc/fs_cli.conf`. On first run with none of them present, `fs_cli` creates a
default config at `~/.config/fs_cli.yaml`.

A legacy file is read only when no YAML one exists, and carries just the keys a
batch run needs — host, port, user, password, debug, loglevel, quiet,
connect-timeout. Everything else in it is listed in a warning and ignored.

Profiles override defaults per-connection:

```yaml
fs_cli:
  default:
    host: localhost
    port: 8021
    password: ClueCon
    log_level: debug
    color: line

  production:
    host: pbx.example.com
    password: secret
    quiet: true
```

## License

MIT OR Apache-2.0
