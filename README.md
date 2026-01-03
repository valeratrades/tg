# tg
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.92+-ab6000.svg)
[<img alt="crates.io" src="https://img.shields.io/crates/v/tg.svg?color=fc8d62&logo=rust" height="20" style=flat-square>](https://crates.io/crates/tg)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs&style=flat-square" height="20">](https://docs.rs/tg)
![Lines Of Code](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/valeratrades/b48e6f02c61942200e7d1e3eeabf9bcb/raw/tg-loc.json)
<br>
[<img alt="ci errors" src="https://img.shields.io/github/actions/workflow/status/valeratrades/tg/errors.yml?branch=master&style=for-the-badge&style=flat-square&label=errors&labelColor=420d09" height="20">](https://github.com/valeratrades/tg/actions?query=branch%3Amaster) <!--NB: Won't find it if repo is private-->
[<img alt="ci warnings" src="https://img.shields.io/github/actions/workflow/status/valeratrades/tg/warnings.yml?branch=master&style=for-the-badge&style=flat-square&label=warnings&labelColor=d16002" height="20">](https://github.com/valeratrades/tg/actions?query=branch%3Amaster) <!--NB: Won't find it if repo is private-->

`tg` tool allows you to interact with a Telegram bot to send messages, get bot information, and list channels configured in your configuration file. I use it to keep my quick notes.
If the broader architecture is of interest, see [ARCHITECTURE.md](./docs/ARCHITECTURE.md).
<!-- markdownlint-disable -->
<details>
<summary>
<h3>Installation</h3>
</summary>

```sh
nix build
```

</details>
<!-- markdownlint-restore -->

## Usage
#### Commands

##### Server
Start a background server that continuously syncs messages from configured forum groups:
```sh
tg server                    # default 1m pull interval
tg server --pull-interval 5m # custom interval
```

The server:
- Discovers all topics in configured forum groups at startup
- Pulls message history via MTProto
- Syncs new messages on the configured interval
- Creates markdown files in `~/.local/share/tg/<group_name>/`

Note: New topics created after server start won't appear until restart.

##### Pull
One-shot pull of messages from all configured forum groups:
```sh
tg pull
```

##### Open
Open a topic file in `$EDITOR` (uses fzf for pattern matching):
```sh
tg open           # fzf over all topics
tg open journal   # open if unique match, else fzf
```

##### Send
Send a message to a topic:
```sh
tg send journal "Today I'm feeling blue"
tg send -g 2244305221 -t 7 "direct by IDs"
```

##### List
List all discovered topics:
```sh
tg list
```

##### Todos
Aggregate TODOs from all topic files:
```sh
tg todos
```

##### Bot Info
Retrieve information about the bot:
```sh
tg bot-info
```

#### Configuration
Create a configuration file at `~/.config/tg.toml`:
```toml
localhost_port = 59753
max_messages_per_chat = 1000

# MTProto credentials (required for pull/server)
# Get these from https://my.telegram.org/
api_id = 12345
api_hash = "your_api_hash"  # or use TELEGRAM_API_HASH env var
phone = "+1234567890"       # or use PHONE_NUMBER_FR env var
username = "@yourusername"  # for session file naming

[groups]
personal = "-1002244305221"        # forum group
work = "-1002244305221/3"          # specific topic in group
```

Groups are specified by chat ID (with -100 prefix for supergroups). Append `/topic_id` to target a specific topic.

Example config: ./examples/config.toml

##### Environment Variables
```sh
export TELEGRAM_MAIN_BOT_TOKEN="your_bot_token"  # for send/bot-info
export TELEGRAM_API_HASH="your_api_hash"         # alternative to config
export PHONE_NUMBER_FR="+1234567890"             # alternative to config
```



<br>

<sup>
	This repository follows <a href="https://github.com/valeratrades/.github/tree/master/best_practices">my best practices</a> and <a href="https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md">Tiger Style</a> (except "proper capitalization for acronyms": (VsrState, not VSRState) and formatting).
</sup>

#### License

<sup>
	Licensed under <a href="LICENSE">GLWTS</a>
</sup>

<br>

<sub>
	Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>

